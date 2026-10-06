//! The arrangement of this computer's monitors, in the OS's global cursor
//! coordinates (points on macOS, physical pixels on Windows; y grows down).
//!
//! The desktop can be any shape: side-by-side, stacked, offset, L-shaped.
//! The cursor crosses to the other computer only at the *outer* edge on the
//! configured side, i.e. where there is no monitor beyond.
//!
//! Positions along an edge travel between computers as a 0..1 fraction of
//! the desktop's bounding box, so the hand-over point stays consistent
//! whatever the two arrangements are.

use crate::protocol::Edge;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
    fn center(&self) -> (f64, f64) {
        ((self.x + self.w / 2.0).floor(), (self.y + self.h / 2.0).floor())
    }
    fn clamp(&self, px: f64, py: f64) -> (f64, f64) {
        (px.clamp(self.x, self.right() - 1.0), py.clamp(self.y, self.bottom() - 1.0))
    }
    fn distance(&self, px: f64, py: f64) -> f64 {
        let (cx, cy) = self.clamp(px, py);
        (cx - px).hypot(cy - py)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Layout {
    pub monitors: Vec<Rect>,
    /// Index of the primary/main monitor.
    pub primary: usize,
}

/// Result of moving the client's virtual cursor.
#[derive(Debug, PartialEq)]
pub enum Step {
    Move(f64, f64),
    /// Left through the edge facing the server, at this position along it.
    Leave(f64),
}

impl Layout {
    pub fn new(mut monitors: Vec<Rect>, primary: usize) -> Self {
        // Mirrored displays report identical bounds; keep one.
        let primary_rect = monitors.get(primary).copied();
        let mut unique: Vec<Rect> = Vec::with_capacity(monitors.len());
        for r in monitors.drain(..) {
            if r.w > 0.0 && r.h > 0.0 && !unique.contains(&r) {
                unique.push(r);
            }
        }
        let mut monitors = unique;
        if monitors.is_empty() {
            monitors.push(Rect::new(0.0, 0.0, 1920.0, 1080.0));
        }
        let primary = primary_rect.and_then(|p| monitors.iter().position(|r| *r == p)).unwrap_or(0);
        Self { monitors, primary }
    }

    /// Reads the current monitor arrangement from the OS.
    #[cfg(feature = "desktop")]
    pub fn detect() -> Self {
        crate::platform::monitors().map(|(m, p)| Self::new(m, p)).unwrap_or_else(|| {
            let (w, h) = crate::platform::screen_size();
            Self::new(vec![Rect::new(0.0, 0.0, w, h)], 0)
        })
    }

    pub fn bbox(&self) -> Rect {
        let x = self.monitors.iter().map(|r| r.x).fold(f64::INFINITY, f64::min);
        let y = self.monitors.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
        let r = self.monitors.iter().map(|r| r.right()).fold(f64::NEG_INFINITY, f64::max);
        let b = self.monitors.iter().map(|r| r.bottom()).fold(f64::NEG_INFINITY, f64::max);
        Rect::new(x, y, r - x, b - y)
    }

    fn monitor_at(&self, x: f64, y: f64) -> Option<&Rect> {
        self.monitors.iter().find(|r| r.contains(x, y))
    }

    fn nearest(&self, x: f64, y: f64) -> &Rect {
        self.monitor_at(x, y).unwrap_or_else(|| {
            self.monitors
                .iter()
                .min_by(|a, b| a.distance(x, y).total_cmp(&b.distance(x, y)))
                .expect("at least one monitor")
        })
    }

    /// Where the server parks its cursor while the client is active.
    pub fn home(&self) -> (f64, f64) {
        self.monitors[self.primary].center()
    }

    /// True if (x, y) is on the outer `edge` of the desktop: on its monitor's
    /// side and with no other monitor beyond it.
    pub fn at_outer_edge(&self, edge: Edge, x: f64, y: f64) -> bool {
        let r = self.nearest(x, y);
        let (on_side, bx, by) = match edge {
            Edge::Left => (x <= r.x, r.x - 1.0, y),
            Edge::Right => (x >= r.right() - 1.0, r.right(), y),
            Edge::Top => (y <= r.y, x, r.y - 1.0),
            Edge::Bottom => (y >= r.bottom() - 1.0, x, r.bottom()),
        };
        on_side && self.monitor_at(bx, by).is_none()
    }

    /// Position along `edge` as a 0..1 fraction of the desktop's extent.
    pub fn edge_pos(&self, edge: Edge, x: f64, y: f64) -> f64 {
        let b = self.bbox();
        let p = match edge {
            Edge::Left | Edge::Right => (y - b.y) / b.h,
            Edge::Top | Edge::Bottom => (x - b.x) / b.w,
        };
        p.clamp(0.0, 1.0)
    }

    /// The point `inset` pixels inside the outer `edge`, at fraction `pos` along it.
    /// Uses the outermost monitor on that side; if the arrangement has a gap
    /// at `pos`, the nearest monitor along the edge is used instead.
    pub fn entry_point(&self, edge: Edge, pos: f64, inset: f64) -> (f64, f64) {
        let b = self.bbox();
        let vertical = matches!(edge, Edge::Left | Edge::Right);
        let along = if vertical { b.y + pos.clamp(0.0, 1.0) * b.h } else { b.x + pos.clamp(0.0, 1.0) * b.w };
        let span = |r: &Rect| if vertical { (r.y, r.bottom()) } else { (r.x, r.right()) };
        let gap = |r: &Rect| {
            let (lo, hi) = span(r);
            if along < lo {
                lo - along
            } else if along >= hi {
                along - hi + 1.0
            } else {
                0.0
            }
        };
        let best_gap = self.monitors.iter().map(gap).fold(f64::INFINITY, f64::min);
        let outward = |r: &Rect| match edge {
            Edge::Left => -r.x,
            Edge::Right => r.right(),
            Edge::Top => -r.y,
            Edge::Bottom => r.bottom(),
        };
        let r = self
            .monitors
            .iter()
            .filter(|r| gap(r) == best_gap)
            .max_by(|a, b| outward(a).total_cmp(&outward(b)))
            .expect("at least one monitor");
        let (lo, hi) = span(r);
        let along = along.clamp(lo, hi - 1.0);
        let inset = inset.min(if vertical { r.w } else { r.h } - 1.0).max(0.0);
        match edge {
            Edge::Left => (r.x + inset, along),
            Edge::Right => (r.right() - 1.0 - inset, along),
            Edge::Top => (along, r.y + inset),
            Edge::Bottom => (along, r.bottom() - 1.0 - inset),
        }
    }

    /// Moves a cursor from `from` toward `to` the way the OS would: across
    /// monitors where they touch, stopped by outer edges, except the edge
    /// facing the server, which hands control back.
    pub fn step(&self, server_edge: Edge, from: (f64, f64), to: (f64, f64)) -> Step {
        if self.monitor_at(to.0, to.1).is_some() {
            return Step::Move(to.0, to.1);
        }
        let r = *self.nearest(from.0, from.1);
        let (cx, cy) = r.clamp(to.0, to.1);
        let pushed_out = match server_edge {
            Edge::Left => to.0 < r.x,
            Edge::Right => to.0 > r.right() - 1.0,
            Edge::Top => to.1 < r.y,
            Edge::Bottom => to.1 > r.bottom() - 1.0,
        };
        if pushed_out && self.at_outer_edge(server_edge, cx, cy) {
            return Step::Leave(self.edge_pos(server_edge, cx, cy));
        }
        // A fast diagonal move can overshoot into a neighbouring monitor's
        // missing corner; land on whichever monitor is closest to the target.
        let n = self.nearest(to.0, to.1);
        let (nx, ny) = n.clamp(to.0, to.1);
        if n != &r && n.distance(to.0, to.1) < r.distance(to.0, to.1) {
            Step::Move(nx, ny)
        } else {
            Step::Move(cx, cy)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single() -> Layout {
        Layout::new(vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)], 0)
    }

    /// Laptop (primary) with a taller external monitor to its right, offset upward.
    ///   [ ext 2560x1440 at (1440,-300) ]
    /// [ laptop 1440x900 at (0,0) ]
    fn laptop_plus_right() -> Layout {
        Layout::new(vec![Rect::new(0.0, 0.0, 1440.0, 900.0), Rect::new(1440.0, -300.0, 2560.0, 1440.0)], 0)
    }

    #[test]
    fn single_monitor_edges() {
        let l = single();
        assert!(l.at_outer_edge(Edge::Right, 1919.0, 500.0));
        assert!(!l.at_outer_edge(Edge::Right, 1917.0, 500.0));
        assert!(l.at_outer_edge(Edge::Left, 0.0, 10.0));
        assert_eq!(l.entry_point(Edge::Left, 0.5, 1.0), (1.0, 540.0));
        assert_eq!(l.home(), (960.0, 540.0));
    }

    #[test]
    fn inner_edges_between_own_monitors_do_not_cross() {
        let l = laptop_plus_right();
        // Laptop's right side touches the external monitor: not an outer edge.
        assert!(!l.at_outer_edge(Edge::Right, 1439.0, 400.0));
        // The external monitor's right side is.
        assert!(l.at_outer_edge(Edge::Right, 3999.0, 0.0));
        // Above the laptop there's nothing: top edge is outer there...
        assert!(l.at_outer_edge(Edge::Top, 100.0, 0.0));
        // ...but the external monitor's top is the outer top edge further right.
        assert!(l.at_outer_edge(Edge::Top, 2000.0, -300.0));
        assert!(!l.at_outer_edge(Edge::Top, 2000.0, 0.0), "y=0 on the external monitor is not its top");
    }

    #[test]
    fn entry_uses_outermost_monitor_and_handles_gaps() {
        let l = laptop_plus_right();
        let b = l.bbox();
        assert_eq!(b, Rect::new(0.0, -300.0, 4000.0, 1440.0));
        // Entering from the left at the very top: only the external monitor reaches that
        // high, and nothing is left of it there, so its left side is the outer edge.
        assert_eq!(l.entry_point(Edge::Left, 0.0, 1.0), (1441.0, -300.0));
        // Lower down, the laptop is outermost on the left.
        assert_eq!(l.entry_point(Edge::Left, 0.5, 1.0), (1.0, 420.0));
        // Entering from the right: the external monitor is outermost.
        let (x, _) = l.entry_point(Edge::Right, 0.5, 1.0);
        assert_eq!(x, 3998.0);
    }

    #[test]
    fn stepping_crosses_own_monitors_and_leaves_only_at_server_edge() {
        let l = laptop_plus_right();
        // Server is to the left of this (client) desktop.
        assert_eq!(l.step(Edge::Left, (1430.0, 400.0), (1460.0, 400.0)), Step::Move(1460.0, 400.0));
        // Pushing right at the far right edge just stops.
        assert_eq!(l.step(Edge::Left, (3990.0, 400.0), (4100.0, 400.0)), Step::Move(3999.0, 400.0));
        // Pushing up off the laptop's top stops (nothing above it).
        assert_eq!(l.step(Edge::Left, (100.0, 5.0), (100.0, -40.0)), Step::Move(100.0, 0.0));
        // Pushing left off the laptop leaves toward the server.
        match l.step(Edge::Left, (3.0, 450.0), (-20.0, 450.0)) {
            Step::Leave(pos) => assert!((pos - 750.0 / 1440.0).abs() < 1e-9),
            other => panic!("expected Leave, got {other:?}"),
        }
    }

    #[test]
    fn positions_round_trip_between_different_layouts() {
        let server = single();
        let client = laptop_plus_right();
        // Server cursor leaves its right edge 25% down; client enters on its left edge.
        let pos = server.edge_pos(Edge::Right, 1919.0, 270.0);
        let (x, y) = client.entry_point(Edge::Left, pos, 1.0);
        assert_eq!(x, 1.0);
        assert!(client.monitor_at(x, y).is_some());
    }

    #[test]
    fn mirrored_and_empty_monitors_are_cleaned_up() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let l = Layout::new(vec![r, r, Rect::new(0.0, 0.0, 0.0, 0.0)], 1);
        assert_eq!(l.monitors.len(), 1);
        assert_eq!(l.primary, 0);
    }
}
