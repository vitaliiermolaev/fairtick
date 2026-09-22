// Stage 7.5 — local lag proxy.
//
//   client → ws://127.0.0.1:9000/ws → LagProxy → ws://127.0.0.1:8080/ws
//
// Inserts deterministic per-direction delay + jitter + occasional burst
// stalls so the acceptance matrix can replay matches under realistic-feeling
// network conditions without hardware. Same `--seed` produces the same trace.
//
// Profiles tunable via `--profile`:
//   Good            zero delay, no jitter (sanity baseline)
//   LossyTCP        40ms each way, ±40ms jitter, 500ms stall every ~20s
//   BadButPlayable  100ms each way, ±80ms jitter, 1000ms stall every ~30s
//
// Trace lines (one per forwarded frame) are appended to
// traces/proxy_<timestamp>.jsonl so the audit pipeline can replay timing.
//
// Usage:
//   cargo run --bin lag_proxy -- --profile LossyTCP --seed 42

use futures_util::{SinkExt, StreamExt};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, Copy)]
enum Profile {
    Good,
    LossyTcp,
    BadButPlayable,
}

impl Profile {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "Good" => Some(Profile::Good),
            "LossyTCP" => Some(Profile::LossyTcp),
            "BadButPlayable" => Some(Profile::BadButPlayable),
            _ => None,
        }
    }

    fn delay_ms(&self) -> u32 {
        match self {
            Profile::Good => 0,
            Profile::LossyTcp => 40,
            Profile::BadButPlayable => 100,
        }
    }
    fn jitter_ms(&self) -> u32 {
        match self {
            Profile::Good => 0,
            Profile::LossyTcp => 40,
            Profile::BadButPlayable => 80,
        }
    }
    fn burst_stall_ms(&self) -> u32 {
        match self {
            Profile::Good => 0,
            Profile::LossyTcp => 500,
            Profile::BadButPlayable => 1000,
        }
    }
    /// Probability per frame that a burst stall starts.
    fn burst_probability(&self) -> f64 {
        match self {
            Profile::Good => 0.0,
            // ~once per 20s assuming ~20 frames/sec snapshot side
            Profile::LossyTcp => 1.0 / 400.0,
            Profile::BadButPlayable => 1.0 / 600.0,
        }
    }
}

#[derive(Clone)]
struct Trace {
    file: Arc<Mutex<std::fs::File>>,
}

impl Trace {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Trace { file: Arc::new(Mutex::new(f)) })
    }

    async fn write(&self, line: &str) {
        use std::io::Write;
        let mut f = self.file.lock().await;
        let _ = writeln!(f, "{}", line);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut profile = Profile::Good;
    let mut seed: u64 = 0xDEAD_BEEF;
    let mut upstream = "ws://127.0.0.1:8080/ws".to_string();
    let mut listen = "127.0.0.1:9000".to_string();

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                profile = Profile::parse(&args[i + 1]).expect("bad profile");
                i += 2;
            }
            "--seed" => {
                seed = args[i + 1].parse().expect("bad seed");
                i += 2;
            }
            "--upstream" => {
                upstream = args[i + 1].clone();
                i += 2;
            }
            "--listen" => {
                listen = args[i + 1].clone();
                i += 2;
            }
            _ => {
                eprintln!("unknown arg: {}", args[i]);
                std::process::exit(2);
            }
        }
    }

    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let trace_path = PathBuf::from(format!("traces/proxy_{}.jsonl", ts));
    let trace = Trace::open(trace_path.clone())?;
    println!(
        "lag_proxy profile={:?} seed={} listen={} -> upstream={} trace={}",
        profile,
        seed,
        listen,
        upstream,
        trace_path.display()
    );

    let listener = TcpListener::bind(&listen).await?;
    let conn_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    loop {
        let (stream, peer) = listener.accept().await?;
        let id = conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let upstream = upstream.clone();
        let trace = trace.clone();
        let conn_seed = seed.wrapping_add(id);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, id, conn_seed, profile, upstream, trace, peer).await {
                eprintln!("conn {} error: {}", id, e);
            }
        });
    }
}

async fn handle(
    stream: tokio::net::TcpStream,
    conn_id: u64,
    seed: u64,
    profile: Profile,
    upstream_url: String,
    trace: Trace,
    peer: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_in = tokio_tungstenite::accept_async(stream).await?;
    let (mut in_write, mut in_read) = ws_in.split();

    let (ws_out, _resp) = tokio_tungstenite::connect_async(&upstream_url).await?;
    let (out_write, mut out_read) = ws_out.split();

    trace
        .write(&format!(
            r#"{{"event":"open","conn":{},"peer":"{}","upstream":"{}","seed":{},"profile":"{:?}"}}"#,
            conn_id, peer, upstream_url, seed, profile
        ))
        .await;

    // Two delay pipelines — each direction has its own RNG so randomness is
    // deterministic per (seed, direction). Channels carry the queued frames
    // along with their release time; a single consumer task per direction
    // sleeps until release and forwards.
    let (c2s_tx, c2s_rx) = mpsc::unbounded_channel::<(Message, std::time::Instant)>();
    let (s2c_tx, s2c_rx) = mpsc::unbounded_channel::<(Message, std::time::Instant)>();

    let mut rng_c2s = ChaCha8Rng::seed_from_u64(seed ^ 0xC25_C0DE);
    let mut rng_s2c = ChaCha8Rng::seed_from_u64(seed ^ 0x52C_C0DE);

    // client → server: read frames, delay, queue.
    let trace_c2s = trace.clone();
    let in_to_out = tokio::spawn(async move {
        while let Some(msg) = in_read.next().await {
            let Ok(msg) = msg else { break };
            let extra = scheduled_delay_ms(profile, &mut rng_c2s);
            let release = std::time::Instant::now() + Duration::from_millis(extra as u64); // allow-wall-clock: proxy scheduling
            trace_c2s.write(&format!(r#"{{"event":"c2s","conn":{},"delay_ms":{}}}"#, conn_id, extra)).await;
            if c2s_tx.send((msg, release)).is_err() {
                break;
            }
        }
        drop(c2s_tx);
    });
    let c2s_pump = spawn_pump(c2s_rx, out_write_box(out_write));

    // server → client.
    let trace_s2c = trace.clone();
    let out_to_in = tokio::spawn(async move {
        while let Some(msg) = out_read.next().await {
            let Ok(msg) = msg else { break };
            let extra = scheduled_delay_ms(profile, &mut rng_s2c);
            let release = std::time::Instant::now() + Duration::from_millis(extra as u64); // allow-wall-clock: proxy scheduling
            trace_s2c.write(&format!(r#"{{"event":"s2c","conn":{},"delay_ms":{}}}"#, conn_id, extra)).await;
            if s2c_tx.send((msg, release)).is_err() {
                break;
            }
        }
        drop(s2c_tx);
    });
    let s2c_pump = spawn_pump_in(s2c_rx, &mut in_write);

    let _ = tokio::join!(in_to_out, out_to_in, c2s_pump, s2c_pump);
    trace.write(&format!(r#"{{"event":"close","conn":{}}}"#, conn_id)).await;
    Ok(())
}

fn scheduled_delay_ms(profile: Profile, rng: &mut ChaCha8Rng) -> u32 {
    let base = profile.delay_ms();
    let jitter = profile.jitter_ms();
    let jitter_add = if jitter == 0 { 0 } else { rng.random_range(0..=jitter) };
    let stall = if rng.random::<f64>() < profile.burst_probability() { profile.burst_stall_ms() } else { 0 };
    base + jitter_add + stall
}

type OutWrite = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

// Wrapping the trait-object-like alias in a Box to avoid type gymnastics with
// the tokio::spawn signature; the actual concrete sink lives behind it.
fn out_write_box(
    w: OutWrite,
) -> futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
> {
    w
}

async fn spawn_pump(mut rx: mpsc::UnboundedReceiver<(Message, std::time::Instant)>, mut sink: OutWrite) {
    while let Some((msg, release)) = rx.recv().await {
        let now = std::time::Instant::now(); // allow-wall-clock: proxy scheduling
        if release > now {
            tokio::time::sleep(release - now).await;
        }
        if sink.send(msg).await.is_err() {
            break;
        }
    }
}

async fn spawn_pump_in(
    mut rx: mpsc::UnboundedReceiver<(Message, std::time::Instant)>,
    sink: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        Message,
    >,
) {
    while let Some((msg, release)) = rx.recv().await {
        let now = std::time::Instant::now(); // allow-wall-clock: proxy scheduling
        if release > now {
            tokio::time::sleep(release - now).await;
        }
        if sink.send(msg).await.is_err() {
            break;
        }
    }
}
