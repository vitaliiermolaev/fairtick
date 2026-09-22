pub mod ai;
pub mod claim_ledger;
pub mod contact_history;
pub mod events;
pub mod filler_bot;
pub mod map;
pub mod outbox;
pub mod player;
pub mod policies;
pub mod rng;
pub mod room;
pub mod room_manager;
pub mod sim;
pub mod snapshot;
pub mod timeline;

#[cfg(test)]
mod regression_tests {
    /// Stage 0 non-negotiable rule: NO gameplay wall-clock.
    /// `Instant::now`, `SystemTime::now`, and `.elapsed()` are forbidden inside
    /// src/game/** because gameplay decisions must be measured in server ticks,
    /// not wall-clock. Wall-clock is allowed in telemetry/logging only.
    ///
    /// This guard test scans the source so a regression is caught the moment a
    /// new gameplay file reintroduces wall-clock semantics.
    #[test]
    fn no_wall_clock_in_game_module() {
        use std::fs;
        use std::path::Path;

        let game_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("game");
        let mut violations = Vec::new();

        // Walk src/game/** recursively: the fairness rules live in the policies/
        // subdirectory, and a flat read_dir silently skipped them. The scanner itself
        // names the banned tokens, so only THIS file (src/game/mod.rs) is excluded.
        let scanner = game_dir.join("mod.rs");
        let mut files = Vec::new();
        let mut dirs = vec![game_dir.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).expect("game dir exists").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().and_then(|s| s.to_str()) == Some("rs") && path != scanner {
                    files.push(path);
                }
            }
        }
        assert!(
            files.iter().any(|f| f.starts_with(game_dir.join("policies"))),
            "the scan must cover src/game/policies"
        );
        for path in files {
            let content = fs::read_to_string(&path).expect("readable");
            for (line_no, line) in content.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with("*") {
                    continue;
                }
                // Stage 7: wall-clock is allowed for telemetry — lines that
                // explicitly opt in with `// allow-wall-clock: <reason>` skip
                // the check. The marker forces a code reviewer to think about
                // why, which is the whole point.
                if line.contains("allow-wall-clock") {
                    continue;
                }
                let banned = ["Instant::now", "SystemTime::now", ".elapsed()"];
                for needle in banned {
                    if line.contains(needle) {
                        violations.push(format!(
                            "{}:{} uses banned wall-clock token `{}` — gameplay must be tick-based (mark with `// allow-wall-clock: <reason>` for telemetry use)",
                            path.display(),
                            line_no + 1,
                            needle
                        ));
                    }
                }
            }
        }

        assert!(violations.is_empty(), "wall-clock usage in src/game/**:\n{}", violations.join("\n"));
    }
}
