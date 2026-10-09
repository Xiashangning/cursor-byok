//! Orders Bidi messages by append sequence number.

use std::collections::BTreeMap;

pub struct OrderedInbox<T> {
    next: i64,
    pending: BTreeMap<i64, T>,
}

impl<T> OrderedInbox<T> {
    pub fn starting_at(next: i64) -> Self {
        Self {
            next,
            pending: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, seqno: i64, value: T) -> Vec<(i64, T)> {
        if seqno < self.next || self.pending.contains_key(&seqno) {
            return Vec::new();
        }
        self.pending.insert(seqno, value);
        let mut ready = Vec::new();
        while let Some(value) = self.pending.remove(&self.next) {
            ready.push((self.next, value));
            self.next = self.next.saturating_add(1);
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L-4 回归:乱序到达的帧在缺口补齐时按序释放,不跳号。
    #[test]
    fn out_of_order_frames_wait_for_their_gap_and_release_in_order() {
        let mut inbox = OrderedInbox::starting_at(0);
        assert_eq!(inbox.push(0, "zero"), vec![(0, "zero")]);
        // 1 缺失:2-4 全部挂起,不跳号。
        assert!(inbox.push(3, "three").is_empty());
        assert!(inbox.push(2, "two").is_empty());
        assert!(inbox.push(4, "four").is_empty());
        // 补齐 1:连续窗口按序一次性释放。
        assert_eq!(
            inbox.push(1, "one"),
            vec![(1, "one"), (2, "two"), (3, "three"), (4, "four")]
        );
    }

    /// L-4 回归:迟到/重复 seqno 丢弃,不回退已释放窗口。
    #[test]
    fn duplicate_and_late_seqnos_are_dropped_without_regressing() {
        let mut inbox = OrderedInbox::starting_at(0);
        assert_eq!(inbox.push(0, "first"), vec![(0, "first")]);
        assert!(inbox.push(0, "duplicate").is_empty());
        // 1 未到但 2 先到:2 挂起;重复的 1 丢弃。
        assert!(inbox.push(2, "two").is_empty());
        assert!(inbox.push(2, "duplicate-two").is_empty());
        assert_eq!(inbox.push(1, "one"), vec![(1, "one"), (2, "two")]);
        // 已释放窗口之后的迟到帧全部丢弃。
        assert!(inbox.push(1, "late-one").is_empty());
        assert!(inbox.push(0, "late-zero").is_empty());
    }
}
