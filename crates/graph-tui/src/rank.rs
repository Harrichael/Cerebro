//! An order with room left in it.
//!
//! A rank is read the way a version number is: dot-separated parts, compared
//! part by part, and a rank that another one begins with sorts first. So `2`
//! comes before `2.5`, which comes before `3`.
//!
//! The point of the shape is that [`Rank::between`] can always answer. Plain
//! integers run out -- there is nothing between 2 and 3 -- and the usual way
//! out is to renumber everything after the insertion, which means a node the
//! user never touched changes its rank. Growing a part instead costs a part
//! and touches nobody: between 2 and 3 is 2.5, and between 2.4 and 2.5 is
//! 2.4.5.

/// Parts run `0..STEPS`, so a rank inserted between neighbours reads as the
/// version number it resembles. A wider base would keep ranks shorter under
/// repeated insertion at one spot, at the cost of parts nobody can read at a
/// glance; ten is plenty, since a rank only grows when a user drops a node
/// into the same gap several times over.
const STEPS: u16 = 10;

/// Where a node stands among its siblings. An order, never a distance.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Rank(Vec<u16>);

impl Rank {
    /// The `n`th of a row being ranked from nothing. Counts from one, not
    /// zero, because nothing sorts before `0`: a rank that another begins
    /// with comes first, so every extension of `0` is *after* it, and a row
    /// starting at zero could never be inserted in front of.
    pub fn nth(n: usize) -> Rank {
        Rank(vec![u16::try_from(n).unwrap_or(u16::MAX - 1).saturating_add(1)])
    }

    /// A rank after `lo` and before `hi`, either end open.
    ///
    /// There is always one. Where two neighbouring parts leave no room the
    /// rank grows a part rather than failing, which is the whole reason it
    /// is a list and not a number.
    pub fn between(lo: Option<&Rank>, hi: Option<&Rank>) -> Rank {
        let lo = lo.map(|r| r.0.as_slice()).unwrap_or(&[]);
        let hi = hi.map(|r| r.0.as_slice());
        // Nothing above means no ceiling, so the next whole number does, and
        // a row added to over and over stays one part long.
        let Some(_) = hi else {
            return Rank(vec![lo.first().copied().unwrap_or(0).saturating_add(1)]);
        };
        let mut out: Vec<u16> = Vec::new();
        for i in 0.. {
            let l = lo.get(i).copied().unwrap_or(0);
            // `hi` stops constraining once a part has gone strictly below it:
            // 1.9 is below 2 whatever follows the 9. Until then it is the
            // ceiling, and past its end there is no ceiling at all.
            let h = match hi {
                Some(h) if out.as_slice() == &h[..out.len().min(h.len())] => {
                    h.get(i).copied().unwrap_or(STEPS)
                }
                _ => STEPS,
            };
            if h > l + 1 {
                out.push(l + (h - l) / 2);
                return Rank(out);
            }
            out.push(l);
        }
        unreachable!("the loop returns as soon as a part has room, and a new part always does")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(parts: &[u16]) -> Rank {
        Rank(parts.to_vec())
    }

    /// The order is the version-number order: part by part, and a rank that
    /// another begins with comes first.
    #[test]
    fn a_rank_sorts_the_way_a_version_number_does() {
        assert!(r(&[2]) < r(&[3]));
        assert!(r(&[2]) < r(&[2, 5]), "a rank sorts before one that extends it");
        assert!(r(&[2, 5]) < r(&[3]));
        assert!(r(&[2, 4]) < r(&[2, 4, 5]));
        assert!(r(&[2, 4, 5]) < r(&[2, 5]));
        assert!(r(&[2, 9, 9]) < r(&[3]), "no depth of 2 reaches 3");
    }

    /// The examples the shape exists for, and the two open ends.
    #[test]
    fn between_finds_room_where_integers_have_none() {
        assert_eq!(Rank::between(Some(&r(&[2])), Some(&r(&[3]))), r(&[2, 5]));
        assert_eq!(Rank::between(Some(&r(&[2, 4])), Some(&r(&[2, 5]))), r(&[2, 4, 5]));
        // Room at this depth is used rather than grown into.
        assert_eq!(Rank::between(Some(&r(&[2])), Some(&r(&[8]))), r(&[5]));

        let first = Rank::between(None, Some(&r(&[4])));
        assert!(first < r(&[4]), "nothing before the first rank");
        let last = Rank::between(Some(&r(&[4])), None);
        assert!(last > r(&[4]), "nothing after the last rank");
    }

    /// The property the whole type is for: dropping a node into the same gap
    /// over and over always has somewhere to go, and never disturbs a rank
    /// that was already there. Fifty insertions at the tightest spot there
    /// is -- always between the first two.
    #[test]
    fn inserting_into_the_same_gap_forever_keeps_the_order() {
        let mut order: Vec<Rank> = (0..3).map(Rank::nth).collect();
        let untouched = order.clone();
        for _ in 0..50 {
            let next = Rank::between(Some(&order[0]), Some(&order[1]));
            order.insert(1, next);
        }
        for pair in order.windows(2) {
            assert!(pair[0] < pair[1], "order broke: {:?} is not before {:?}", pair[0], pair[1]);
        }
        assert_eq!(order.first(), untouched.first(), "the rank before the gap moved");
        assert_eq!(order.last(), untouched.last(), "a rank nowhere near the gap moved");
        assert_eq!(order.len(), 53);
    }

    /// Insertion at the end and at the front are the cases a plain integer
    /// handles and must not be made worse: they stay one part long.
    #[test]
    fn appending_and_prepending_do_not_grow_a_rank() {
        let mut order: Vec<Rank> = (0..3).map(Rank::nth).collect();
        for _ in 0..40 {
            let next = Rank::between(order.last(), None);
            order.push(next);
        }
        let front = Rank::between(None, order.first());
        assert!(front < order[0], "nothing sorts before the first rank");
        order.insert(0, front);
        assert!(order[1..].iter().all(|k| k.0.len() == 1), "appending grew a rank: {order:?}");
        for pair in order.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }
}
