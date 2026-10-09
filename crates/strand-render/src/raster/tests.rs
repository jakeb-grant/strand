use super::*;

#[test]
fn disjoint_covers_exactly_without_overlap() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move |n: i64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as i64
    };
    for _ in 0..500 {
        let mut d = Damage::new();
        for _ in 0..rnd(12) {
            d.add(Rect::new(
                rnd(60) as i32,
                rnd(60) as i32,
                rnd(30) as u32,
                rnd(30) as u32,
            ));
        }
        let parts = disjoint(&d);
        let sum: u64 = parts.iter().map(|r| r.area()).sum();
        assert_eq!(sum, d.area(), "{d:?} -> {parts:?}");
        for (i, a) in parts.iter().enumerate() {
            assert!(d.covers(*a));
            for b in &parts[i + 1..] {
                assert!(!a.intersects(*b), "{a:?} overlaps {b:?}");
            }
        }
    }
}
