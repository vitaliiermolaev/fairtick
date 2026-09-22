//! Typed timeline wrappers.
//!
//! Real-time fairness bugs are almost always *timeline* bugs (see the
//! `deaths-are-presentation-skew` note): the same bare number can mean an authoritative
//! server tick, a fractional client *render* tick, or wall-clock milliseconds, and
//! silently mixing them is exactly how an enemy gets rendered ~delay ticks behind its
//! true position and a player "dies unfairly". These newtypes make the compiler reject a
//! `UnixMs` where a `ServerTick` is expected.
//!
//! Introduced *narrowly* — at policy boundaries — rather than swept across every
//! signature (see Plan.md). Keep them lightweight: no surprise arithmetic, no Deref.
//!
//! Serde derives are transparent (a newtype struct serializes as its inner value), so
//! `ServerTick(7)` is just `7` on the wire / in a fixture.

use serde::{Deserialize, Serialize};

/// Authoritative server simulation tick. Monotonic, exactly one per `Room::update`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerTick(pub u64);

impl ServerTick {
    #[inline]
    pub const fn new(t: u64) -> Self {
        Self(t)
    }
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl ServerTick {
    /// This server tick placed on the render timeline. Both timelines count the same
    /// 1/tick_rate steps (a render tick is fractional and trails the server), so the value
    /// carries over unchanged; the method exists so every crossing is explicit. Use it only
    /// to order a server-side event against client render ticks, e.g. a filler kill (a filler
    /// has no render timeline) in the claim ledger.
    #[inline]
    pub fn as_render_tick(self) -> RenderTick {
        RenderTick(self.0 as f64)
    }
}

impl std::fmt::Display for ServerTick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for ServerTick {
    #[inline]
    fn from(t: u64) -> Self {
        Self(t)
    }
}
impl From<ServerTick> for u64 {
    #[inline]
    fn from(t: ServerTick) -> u64 {
        t.0
    }
}

/// A *client* render tick — fractional, in Unity tick units. This is the timeline the
/// attacker actually saw: interpolated, `delay` ticks behind the server. Used to
/// reconstruct the attacker's view when validating an EatClaim.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RenderTick(pub f64);

impl RenderTick {
    #[inline]
    pub const fn new(t: f64) -> Self {
        Self(t)
    }
    #[inline]
    pub const fn get(self) -> f64 {
        self.0
    }
    /// The server tick at or below this render tick: the "floor" frame the contact-history
    /// ring is sampled against. Clamped at 0.
    #[inline]
    pub fn floor_tick(self) -> ServerTick {
        ServerTick(self.0.floor().max(0.0) as u64)
    }
    /// How many ticks this render tick lies after `earlier` on the same render timeline
    /// (negative when it is before). A duration in ticks, not a point on any timeline.
    #[inline]
    pub fn ticks_after(self, earlier: RenderTick) -> f64 {
        self.0 - earlier.0
    }
    /// The render tick `ticks` earlier on the same timeline.
    #[inline]
    pub fn earlier_by(self, ticks: f64) -> RenderTick {
        RenderTick(self.0 - ticks)
    }
    #[inline]
    pub fn is_finite(self) -> bool {
        self.0.is_finite()
    }
}

impl std::fmt::Display for RenderTick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.3}", self.0)
    }
}

/// Wall-clock milliseconds since the Unix epoch. Telemetry / time-sync ONLY — never a
/// gameplay input (gameplay is tick-based; the `no_wall_clock_in_game_module` guard in
/// `game/mod.rs` enforces this for the simulation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnixMs(pub u64);

impl UnixMs {
    #[inline]
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for UnixMs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_tick_roundtrips_and_orders() {
        assert_eq!(ServerTick::new(7).get(), 7);
        assert_eq!(u64::from(ServerTick::from(42u64)), 42);
        assert!(ServerTick(3) < ServerTick(4));
    }

    #[test]
    fn render_tick_floors_to_server_tick() {
        assert_eq!(RenderTick::new(1500.9).floor_tick(), ServerTick::new(1500));
        assert_eq!(RenderTick::new(0.4).floor_tick(), ServerTick::new(0));
        // Negative render ticks clamp at 0 rather than wrapping a u64.
        assert_eq!(RenderTick::new(-3.0).floor_tick(), ServerTick::new(0));
    }

    #[test]
    fn explicit_timeline_conversions() {
        assert_eq!(RenderTick::new(1000.0).ticks_after(RenderTick::new(992.5)), 7.5);
        assert_eq!(RenderTick::new(992.5).ticks_after(RenderTick::new(1000.0)), -7.5);
        assert_eq!(RenderTick::new(100.0).earlier_by(96.0), RenderTick::new(4.0));
        assert_eq!(ServerTick::new(1234).as_render_tick(), RenderTick::new(1234.0));
        assert!(!RenderTick::new(f64::NAN).is_finite());
    }
}
