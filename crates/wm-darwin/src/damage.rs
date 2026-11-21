//! Screen damage: pure rectangle bookkeeping, no rendering and no devices.
//!
//! A frame is described as an ordered display list of [`ItemSig`]s (bottom to
//! top). Damage is the difference between the list that is on screen and the
//! list that should be: anything that appeared, vanished, moved, changed
//! content or changed stacking. Callers never have to report damage at each
//! mutation site, so a forgotten report cannot leave a stale region.

/// Half-open pixel rectangle `[x0, x1) x [y0, y1)` in output coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}
impl DRect {
    pub const EMPTY: DRect = DRect { x0: 0, y0: 0, x1: 0, y1: 0 };
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }
    /// Saturating, so hostile sizes cannot wrap into a small rectangle.
    pub fn from_xywh(x: i32, y: i32, w: u32, h: u32) -> Self {
        let far = |origin: i32, len: u32| {
            (i64::from(origin) + i64::from(len)).min(i64::from(i32::MAX)) as i32
        };
        Self { x0: x, y0: y, x1: far(x, w), y1: far(y, h) }
    }
    pub fn from_rect(r: wm_core::Rect) -> Self {
        Self::from_xywh(r.pos.x, r.pos.y, r.size.w, r.size.h)
    }
    pub fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
    pub fn width(self) -> u32 {
        if self.is_empty() { 0 } else { (i64::from(self.x1) - i64::from(self.x0)) as u32 }
    }
    pub fn height(self) -> u32 {
        if self.is_empty() { 0 } else { (i64::from(self.y1) - i64::from(self.y0)) as u32 }
    }
    pub fn area(self) -> u64 {
        u64::from(self.width()) * u64::from(self.height())
    }
    pub fn intersect(self, o: DRect) -> DRect {
        let r = DRect {
            x0: self.x0.max(o.x0),
            y0: self.y0.max(o.y0),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        };
        if r.is_empty() { DRect::EMPTY } else { r }
    }
    pub fn intersects(self, o: DRect) -> bool {
        !self.intersect(o).is_empty()
    }
    /// Bounding box. An empty operand contributes nothing.
    pub fn union(self, o: DRect) -> DRect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        DRect {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }
    pub fn translate(self, dx: i32, dy: i32) -> DRect {
        DRect {
            x0: self.x0.saturating_add(dx),
            y0: self.y0.saturating_add(dy),
            x1: self.x1.saturating_add(dx),
            y1: self.y1.saturating_add(dy),
        }
    }
    pub fn contains_point(self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// Most rectangles a region keeps before it starts merging the cheapest pair.
pub const MAX_RECTS: usize = 16;
/// Two rectangles are merged when their bounding box wastes at most this many
/// pixels: one slightly larger repaint beats two loop set-ups.
pub const MERGE_SLACK_PX: u64 = 4096;

/// A small set of rectangles clipped to the output. Rectangles may overlap;
/// repainting a pixel twice is harmless because every repaint recomposites it.
#[derive(Clone, Debug)]
pub struct Region {
    bounds: DRect,
    rects: Vec<DRect>,
}
impl Region {
    pub fn new(width: u32, height: u32) -> Self {
        Self { bounds: DRect::from_xywh(0, 0, width, height), rects: Vec::new() }
    }
    pub fn bounds(&self) -> DRect {
        self.bounds
    }
    pub fn rects(&self) -> &[DRect] {
        &self.rects
    }
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }
    pub fn clear(&mut self) {
        self.rects.clear();
    }
    /// Sum of rectangle areas (overlaps counted twice): the work to be done.
    pub fn area(&self) -> u64 {
        self.rects.iter().map(|r| r.area()).sum()
    }
    pub fn add_all(&mut self) {
        self.rects.clear();
        if !self.bounds.is_empty() {
            self.rects.push(self.bounds);
        }
    }
    pub fn add_region(&mut self, other: &Region) {
        for r in &other.rects {
            self.add(*r);
        }
    }
    pub fn add(&mut self, rect: DRect) {
        let mut rect = rect.intersect(self.bounds);
        if rect.is_empty() {
            return;
        }
        // Absorb every rectangle that is cheap to merge; the grown rectangle
        // may then reach others, so start over after each merge.
        'again: loop {
            for i in 0..self.rects.len() {
                let other = self.rects[i];
                let joined = rect.union(other);
                if joined.area() <= rect.area() + other.area() + MERGE_SLACK_PX {
                    rect = joined;
                    self.rects.swap_remove(i);
                    continue 'again;
                }
            }
            break;
        }
        self.rects.push(rect);
        while self.rects.len() > MAX_RECTS {
            self.merge_cheapest_pair();
        }
    }
    fn merge_cheapest_pair(&mut self) {
        let mut best = (0, 1, u64::MAX);
        for i in 0..self.rects.len() {
            for j in i + 1..self.rects.len() {
                let (a, b) = (self.rects[i], self.rects[j]);
                let waste = a.union(b).area().saturating_sub(a.area() + b.area());
                if waste < best.2 {
                    best = (i, j, waste);
                }
            }
        }
        let joined = self.rects[best.0].union(self.rects[best.1]);
        self.rects.swap_remove(best.1);
        self.rects.swap_remove(best.0);
        // Re-enter add() so the grown rectangle absorbs what it now covers.
        self.add(joined);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKey {
    Background,
    Shell(u64),
    /// Client pixels of a window.
    Window(u64),
    /// Decoration frame of a window (keyed by window, one frame per window).
    Decoration(u64),
    Cursor,
}

/// What is drawn where, and which version of its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemSig {
    pub key: ItemKey,
    /// Everything this item can touch on the output.
    pub rect: DRect,
    /// Changes whenever the item's pixels change.
    pub generation: u64,
}

/// Add to `region` every pixel that may differ between two display lists.
///
/// `partial(key)` may return output-space rectangles that bound the content
/// change of an item whose generation changed but whose rectangle did not;
/// `None` means the whole item.
pub fn diff(
    old: &[ItemSig],
    new: &[ItemSig],
    mut partial: impl FnMut(ItemKey) -> Option<Vec<DRect>>,
    region: &mut Region,
) {
    let find = |list: &[ItemSig], key: ItemKey| list.iter().position(|i| i.key == key);
    for o in old {
        if find(new, o.key).is_none() {
            region.add(o.rect);
        }
    }
    for n in new {
        let Some(at) = find(old, n.key) else {
            region.add(n.rect);
            continue;
        };
        let o = old[at];
        if o.rect != n.rect {
            region.add(o.rect);
            region.add(n.rect);
        } else if o.generation != n.generation {
            match partial(n.key) {
                Some(rects) => {
                    for r in rects {
                        region.add(r.intersect(n.rect));
                    }
                }
                None => region.add(n.rect),
            }
        }
    }
    // Stacking. Compare the order of the items both lists share: when a pair
    // swaps, at least one of the two sits at a different rank, and damaging
    // every item whose rank changed therefore covers every swapped overlap.
    let old_common: Vec<ItemKey> =
        old.iter().filter(|o| find(new, o.key).is_some()).map(|o| o.key).collect();
    let mut rank = 0;
    for n in new {
        if find(old, n.key).is_none() {
            continue;
        }
        if old_common.get(rank) != Some(&n.key) {
            region.add(n.rect);
        }
        rank += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covered(region: &Region, x: i32, y: i32) -> bool {
        region.rects().iter().any(|r| r.contains_point(x, y))
    }
    /// Deterministic generator; no external crate and no clock.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u32) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as u32) % bound.max(1)
        }
    }

    #[test]
    fn rect_basics_and_saturation() {
        let r = DRect::from_xywh(10, 20, 30, 40);
        assert_eq!((r.width(), r.height(), r.area()), (30, 40, 1200));
        assert!(DRect::from_xywh(5, 5, 0, 9).is_empty());
        assert_eq!(DRect::from_xywh(i32::MAX - 1, 0, u32::MAX, 1).x1, i32::MAX);
        assert_eq!(r.intersect(DRect::new(0, 0, 15, 25)), DRect::new(10, 20, 15, 25));
        assert_eq!(r.intersect(DRect::new(100, 100, 200, 200)), DRect::EMPTY);
        assert_eq!(DRect::EMPTY.union(r), r);
        assert_eq!(r.union(DRect::new(0, 0, 1, 1)), DRect::new(0, 0, 40, 60));
    }

    #[test]
    fn region_clips_to_output_and_drops_outside() {
        let mut region = Region::new(100, 50);
        region.add(DRect::new(-20, -20, 10, 10));
        region.add(DRect::new(200, 0, 300, 10));
        region.add(DRect::new(90, 40, 500, 500));
        for r in region.rects() {
            assert_eq!(r.intersect(region.bounds()), *r);
        }
        assert!(covered(&region, 0, 0) && covered(&region, 99, 49));
        assert!(!region.rects().iter().any(|r| r.is_empty()));
    }

    #[test]
    fn pointer_step_merges_but_distant_rects_stay_separate() {
        // A 12x18 cursor moving 3 px: one rectangle, not two.
        let mut region = Region::new(2560, 1600);
        region.add(DRect::from_xywh(500, 500, 12, 18));
        region.add(DRect::from_xywh(503, 502, 12, 18));
        assert_eq!(region.rects(), &[DRect::new(500, 500, 515, 520)]);
        // A jump across the screen must not repaint everything in between.
        let mut region = Region::new(2560, 1600);
        region.add(DRect::from_xywh(10, 10, 12, 18));
        region.add(DRect::from_xywh(2000, 1400, 12, 18));
        assert_eq!(region.rects().len(), 2);
        assert_eq!(region.area(), 2 * 12 * 18);
    }

    #[test]
    fn full_damage_absorbs_everything() {
        let mut region = Region::new(640, 480);
        region.add(DRect::new(1, 1, 5, 5));
        region.add_all();
        region.add(DRect::new(100, 100, 200, 200));
        assert_eq!(region.rects(), &[DRect::new(0, 0, 640, 480)]);
        assert_eq!(region.area(), 640 * 480);
    }

    #[test]
    fn random_adds_always_cover_and_stay_bounded() {
        let mut rng = Lcg(0x5eed);
        for round in 0..200 {
            let mut region = Region::new(320, 240);
            let mut added = Vec::new();
            for _ in 0..(1 + rng.next(60)) {
                let r = DRect::from_xywh(
                    rng.next(400) as i32 - 40,
                    rng.next(300) as i32 - 30,
                    rng.next(if round % 2 == 0 { 20 } else { 200 }),
                    rng.next(if round % 2 == 0 { 20 } else { 200 }),
                );
                region.add(r);
                added.push(r.intersect(region.bounds()));
                assert!(region.rects().len() <= MAX_RECTS);
            }
            for r in &added {
                // Corners and centre of every input stay covered.
                if r.is_empty() {
                    continue;
                }
                for (x, y) in [
                    (r.x0, r.y0),
                    (r.x1 - 1, r.y0),
                    (r.x0, r.y1 - 1),
                    (r.x1 - 1, r.y1 - 1),
                    ((r.x0 + r.x1) / 2, (r.y0 + r.y1) / 2),
                ] {
                    assert!(covered(&region, x, y), "lost {r:?} in {:?}", region.rects());
                }
            }
            // Exhaustive pixel check against a reference bitmap.
            let mut want = vec![false; 320 * 240];
            for r in &added {
                for y in r.y0..r.y1 {
                    for x in r.x0..r.x1 {
                        want[y as usize * 320 + x as usize] = true;
                    }
                }
            }
            for y in 0..240 {
                for x in 0..320 {
                    if want[y as usize * 320 + x as usize] {
                        assert!(covered(&region, x, y));
                    }
                }
            }
        }
    }

    fn sig(key: ItemKey, r: DRect, generation: u64) -> ItemSig {
        ItemSig { key, rect: r, generation }
    }
    fn run(old: &[ItemSig], new: &[ItemSig]) -> Region {
        let mut region = Region::new(1000, 1000);
        diff(old, new, |_| None, &mut region);
        region
    }

    #[test]
    fn identical_lists_have_no_damage() {
        let list = [
            sig(ItemKey::Background, DRect::new(0, 0, 1000, 1000), 1),
            sig(ItemKey::Window(1), DRect::new(10, 10, 110, 110), 4),
            sig(ItemKey::Cursor, DRect::new(5, 5, 17, 23), 1),
        ];
        assert!(run(&list, &list).is_empty());
    }

    #[test]
    fn pointer_motion_damages_old_and_new_cursor_only() {
        let bg = sig(ItemKey::Background, DRect::new(0, 0, 1000, 1000), 1);
        let old = [bg, sig(ItemKey::Cursor, DRect::from_xywh(100, 100, 12, 18), 1)];
        let new = [bg, sig(ItemKey::Cursor, DRect::from_xywh(700, 800, 12, 18), 1)];
        let region = run(&old, &new);
        assert_eq!(region.area(), 2 * 12 * 18);
        assert!(covered(&region, 100, 100) && covered(&region, 711, 817));
        assert!(!covered(&region, 400, 400));
    }

    #[test]
    fn appear_vanish_move_resize_and_content_change() {
        let bg = sig(ItemKey::Background, DRect::new(0, 0, 1000, 1000), 1);
        let a = sig(ItemKey::Window(1), DRect::new(100, 100, 200, 200), 1);
        // Appear and vanish.
        assert_eq!(run(&[bg], &[bg, a]).rects(), &[a.rect]);
        assert_eq!(run(&[bg, a], &[bg]).rects(), &[a.rect]);
        // Move far: both places, nothing else.
        let moved = sig(ItemKey::Window(1), DRect::new(600, 600, 700, 700), 1);
        let region = run(&[bg, a], &[bg, moved]);
        assert_eq!(region.area(), 2 * 100 * 100);
        // Resize: the larger of the two extents is covered.
        let grown = sig(ItemKey::Window(1), DRect::new(100, 100, 300, 250), 1);
        let region = run(&[bg, a], &[bg, grown]);
        assert!(covered(&region, 299, 249) && covered(&region, 100, 100));
        // Content only: exactly the item.
        let repainted = sig(ItemKey::Window(1), a.rect, 2);
        assert_eq!(run(&[bg, a], &[bg, repainted]).rects(), &[a.rect]);
        // Background change: everything.
        let bg2 = sig(ItemKey::Background, bg.rect, 2);
        assert_eq!(run(&[bg, a], &[bg2, a]).rects(), &[bg.rect]);
    }

    #[test]
    fn partial_content_damage_is_used_only_for_unmoved_items() {
        let a = sig(ItemKey::Window(1), DRect::new(100, 100, 900, 700), 1);
        let a2 = sig(ItemKey::Window(1), a.rect, 2);
        let line = DRect::new(100, 300, 900, 316);
        let mut region = Region::new(1000, 1000);
        diff(&[a], &[a2], |_| Some(vec![line, DRect::new(0, 0, 5000, 5000)]), &mut region);
        // The second rectangle is clipped to the item, so the item is the bound.
        assert_eq!(region.rects(), &[a.rect]);
        let mut region = Region::new(1000, 1000);
        diff(&[a], &[a2], |_| Some(vec![line]), &mut region);
        assert_eq!(region.rects(), &[line]);
        // Moved at the same time: partial information is not consulted.
        let moved = sig(ItemKey::Window(1), a.rect.translate(50, 0), 2);
        let mut region = Region::new(1000, 1000);
        diff(&[a], &[moved], |_| panic!("partial must not be asked"), &mut region);
        assert!(covered(&region, 100, 100) && covered(&region, 949, 699));
    }

    #[test]
    fn restacking_damages_every_swapped_overlap() {
        let mut rng = Lcg(42);
        for _ in 0..300 {
            let n = 2 + rng.next(6) as usize;
            let items: Vec<ItemSig> = (0..n)
                .map(|i| {
                    sig(
                        ItemKey::Window(i as u64),
                        DRect::from_xywh(
                            rng.next(800) as i32,
                            rng.next(800) as i32,
                            50 + rng.next(300),
                            50 + rng.next(300),
                        ),
                        1,
                    )
                })
                .collect();
            let mut shuffled = items.clone();
            for i in (1..n).rev() {
                shuffled.swap(i, rng.next(i as u32 + 1) as usize);
            }
            let region = run(&items, &shuffled);
            let pos = |list: &[ItemSig], key| list.iter().position(|i| i.key == key).unwrap();
            for a in &items {
                for b in &items {
                    let flipped = (pos(&items, a.key) < pos(&items, b.key))
                        != (pos(&shuffled, a.key) < pos(&shuffled, b.key));
                    let overlap = a.rect.intersect(b.rect).intersect(region.bounds());
                    if a.key != b.key && flipped && !overlap.is_empty() {
                        assert!(covered(&region, overlap.x0, overlap.y0));
                        assert!(covered(&region, overlap.x1 - 1, overlap.y1 - 1));
                    }
                }
            }
            if shuffled == items {
                assert!(region.is_empty());
            }
        }
    }

    #[test]
    fn raising_one_window_leaves_unrelated_windows_alone() {
        let a = sig(ItemKey::Window(1), DRect::new(0, 0, 100, 100), 1);
        let b = sig(ItemKey::Window(2), DRect::new(500, 500, 600, 600), 1);
        let c = sig(ItemKey::Window(3), DRect::new(800, 0, 900, 100), 1);
        // c stays on top; a and b swap.
        let region = run(&[a, b, c], &[b, a, c]);
        assert!(!covered(&region, 850, 50));
        assert_eq!(region.area(), 2 * 100 * 100);
    }
}
