//! Play queue: a context (album/playlist) with shuffle/repeat plus a user "up next" list.

use std::collections::{HashSet, VecDeque};

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

use crate::model::{RepeatMode, Track};

#[derive(Default, Clone)]
pub struct Queue {
    /// Tracks of the current context in their original order.
    tracks: Vec<Track>,
    /// Play order as indices into `tracks`.
    order: Vec<usize>,
    /// Position in `order` of the current context track.
    pos: Option<usize>,
    /// Manually queued tracks, played before the context continues.
    up_next: VecDeque<Track>,
    /// The playing track (may come from `up_next`).
    current: Option<Track>,
    /// Whether `current` came from `up_next` (the context is still at `pos`).
    current_queued: bool,
    /// Context tracks taken out of the queue (removed or cleared): they stay out when the
    /// play order is made again (shuffle on or off).
    removed: HashSet<usize>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub context_name: String,
}

/// The whole queue as song ids, for saving between runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueState {
    pub tracks: Vec<String>,
    pub order: Vec<usize>,
    pub pos: Option<usize>,
    pub up_next: Vec<String>,
    pub current: Option<String>,
    pub current_queued: bool,
    pub removed: Vec<usize>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub context_name: String,
}

impl Queue {
    pub fn state(&self) -> QueueState {
        let mut removed: Vec<usize> = self.removed.iter().copied().collect();
        removed.sort_unstable();
        QueueState {
            tracks: self.tracks.iter().map(|t| t.id.clone()).collect(),
            order: self.order.clone(),
            pos: self.pos,
            up_next: self.up_next.iter().map(|t| t.id.clone()).collect(),
            current: self.current.as_ref().map(|t| t.id.clone()),
            current_queued: self.current_queued,
            removed,
            shuffle: self.shuffle,
            repeat: self.repeat,
            context_name: self.context_name.clone(),
        }
    }

    /// Puts back a saved queue, `song` turning ids into songs. False (and nothing changed)
    /// when a song of the playlist or the current one is gone, or the state doesn't add up.
    pub fn restore(&mut self, state: QueueState, song: impl Fn(&str) -> Option<Track>) -> bool {
        let Some(tracks) = state.tracks.iter().map(|id| song(id)).collect::<Option<Vec<Track>>>() else {
            return false;
        };
        let current = match &state.current {
            Some(id) => match song(id) {
                Some(t) => Some(t),
                None => return false,
            },
            None => None,
        };
        let n = tracks.len();
        let fits =
            state.order.iter().chain(&state.removed).all(|&i| i < n) && state.pos.is_none_or(|p| p < state.order.len());
        if !fits {
            return false;
        }
        self.tracks = tracks;
        self.order = state.order;
        self.pos = state.pos;
        // Songs you queued that are gone are just left out.
        self.up_next = state.up_next.iter().filter_map(|id| song(id)).collect();
        self.current = current;
        self.current_queued = state.current_queued;
        self.removed = state.removed.into_iter().collect();
        self.shuffle = state.shuffle;
        self.repeat = state.repeat;
        self.context_name = state.context_name;
        true
    }

    /// The first song of the list playback started from.
    pub fn context_first(&self) -> Option<&Track> {
        self.tracks.first()
    }

    /// Every song the queue holds.
    pub fn all_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().chain(&self.up_next).chain(&self.current)
    }

    pub fn set_context(&mut self, tracks: Vec<Track>, start: usize, name: String) -> Option<Track> {
        self.tracks = tracks;
        self.context_name = name;
        self.removed.clear();
        self.current_queued = false;
        if self.tracks.is_empty() {
            self.order.clear();
            self.pos = None;
            self.current = None;
            return None;
        }
        let start = start.min(self.tracks.len() - 1);
        self.build_order(start);
        self.current = Some(self.tracks[start].clone());
        self.current.clone()
    }

    fn build_order(&mut self, current: usize) {
        let removed = &self.removed;
        self.order = (0..self.tracks.len())
            .filter(|i| *i == current || !removed.contains(i))
            .collect();
        if self.shuffle {
            self.order.retain(|&i| i != current);
            self.order.shuffle(&mut rand::rng());
            self.order.insert(0, current);
            self.pos = Some(0);
        } else {
            self.pos = self.order.iter().position(|&i| i == current);
        }
    }

    pub fn current(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    /// Index into the original context of the current context track.
    fn current_context_index(&self) -> Option<usize> {
        self.pos.and_then(|p| self.order.get(p).copied())
    }

    pub fn set_shuffle(&mut self, on: bool) {
        if self.shuffle == on {
            return;
        }
        self.shuffle = on;
        if let Some(cur) = self.current_context_index() {
            self.build_order(cur);
        }
    }

    /// What `advance(false)` would return, without changing state. Used for preloading.
    pub fn peek_next(&self) -> Option<&Track> {
        if self.repeat == RepeatMode::One {
            return self.current.as_ref();
        }
        if let Some(t) = self.up_next.front() {
            return Some(t);
        }
        let next = self.pos.map_or(0, |p| p + 1);
        if let Some(&i) = self.order.get(next) {
            return self.tracks.get(i);
        }
        if self.repeat == RepeatMode::All {
            return self.order.first().and_then(|&i| self.tracks.get(i));
        }
        None
    }

    /// Moves to the next track. `user` is true for an explicit "next" press, which
    /// skips over repeat-one.
    pub fn advance(&mut self, user: bool) -> Option<Track> {
        if !user && self.repeat == RepeatMode::One && self.current.is_some() {
            return self.current.clone();
        }
        if let Some(t) = self.up_next.pop_front() {
            self.current = Some(t);
            self.current_queued = true;
            return self.current.clone();
        }
        self.current_queued = false;
        let next = self.pos.map_or(0, |p| p + 1);
        if next < self.order.len() {
            self.pos = Some(next);
        } else if self.repeat != RepeatMode::Off && !self.order.is_empty() {
            if self.shuffle {
                // Reshuffle for the next round.
                self.order.shuffle(&mut rand::rng());
            }
            self.pos = Some(0);
        } else {
            self.current = None;
            return None;
        }
        self.current = self.current_context_index().map(|i| self.tracks[i].clone());
        self.current.clone()
    }

    pub fn previous(&mut self) -> Option<Track> {
        // From a song you queued, back to the one that played before it.
        if self.current_queued {
            if let Some(i) = self.current_context_index() {
                self.current_queued = false;
                self.current = Some(self.tracks[i].clone());
                return self.current.clone();
            }
        }
        match self.pos {
            Some(p) if p > 0 => self.pos = Some(p - 1),
            Some(_) if self.repeat == RepeatMode::All && !self.order.is_empty() => {
                self.pos = Some(self.order.len() - 1)
            }
            _ => return self.current.clone(),
        }
        self.current = self.current_context_index().map(|i| self.tracks[i].clone());
        self.current.clone()
    }

    /// Swaps in new versions of songs (after their tags were edited, say).
    pub fn refresh(&mut self, updated: &std::collections::HashMap<String, Track>) {
        let swap = |t: &mut Track| {
            if let Some(new) = updated.get(&t.id) {
                *t = new.clone();
            }
        };
        self.tracks.iter_mut().for_each(swap);
        self.up_next.iter_mut().for_each(swap);
        self.current.iter_mut().for_each(swap);
    }

    pub fn enqueue(&mut self, tracks: Vec<Track>) {
        self.up_next.extend(tracks);
    }

    pub fn play_next(&mut self, tracks: Vec<Track>) {
        for t in tracks.into_iter().rev() {
            self.up_next.push_front(t);
        }
    }

    /// Upcoming tracks in play order: manual queue first, then the rest of the context.
    pub fn upcoming(&self, limit: usize) -> Vec<Track> {
        let rest = self.pos.map_or(0, |p| p + 1);
        self.up_next
            .iter()
            .cloned()
            .chain(self.order.iter().skip(rest).map(|&i| self.tracks[i].clone()))
            .take(limit)
            .collect()
    }

    pub fn up_next_len(&self) -> usize {
        self.up_next.len()
    }

    /// Where song `id` is in `upcoming()`: at `index` if it's still there (the queue may have
    /// moved on since it was shown), else its first entry.
    fn find_upcoming(&self, index: usize, id: &str) -> Option<usize> {
        let rest = self.pos.map_or(0, |p| p + 1);
        let at = |i: usize| -> Option<&Track> {
            if i < self.up_next.len() {
                self.up_next.get(i)
            } else {
                self.order.get(rest + i - self.up_next.len()).map(|&t| &self.tracks[t])
            }
        };
        if at(index).is_some_and(|t| t.id == id) {
            return Some(index);
        }
        (0..self.up_next.len() + self.order.len().saturating_sub(rest)).find(|&i| at(i).is_some_and(|t| t.id == id))
    }

    /// Jumps to song `id` at entry `index` of `upcoming()`. Songs you queued stay queued when
    /// jumping into the rest of the playlist.
    pub fn jump_to(&mut self, index: usize, id: &str) -> Option<Track> {
        let index = self.find_upcoming(index, id)?;
        if index < self.up_next.len() {
            self.up_next.drain(..index);
            return self.advance(true);
        }
        let rest = self.pos.map_or(0, |p| p + 1);
        let target = rest + index - self.up_next.len();
        if target >= self.order.len() {
            return None;
        }
        self.pos = Some(target);
        self.current_queued = false;
        self.current = self.current_context_index().map(|i| self.tracks[i].clone());
        self.current.clone()
    }

    /// Removes song `id` at entry `index` of `upcoming()`.
    pub fn remove(&mut self, index: usize, id: &str) {
        let Some(index) = self.find_upcoming(index, id) else {
            return;
        };
        if index < self.up_next.len() {
            self.up_next.remove(index);
            return;
        }
        let rest = self.pos.map_or(0, |p| p + 1);
        let target = rest + index - self.up_next.len();
        if target < self.order.len() {
            self.removed.insert(self.order.remove(target));
        }
    }

    /// Drops the rest of the playlist (songs you queued stay).
    pub fn clear_upcoming(&mut self) {
        let keep = self.pos.map_or(0, |p| p + 1);
        if keep < self.order.len() {
            self.removed.extend(self.order.drain(keep..));
        }
    }

    /// Drops the songs you queued.
    pub fn clear_queued(&mut self) {
        self.up_next.clear();
    }

    /// Moves song `id` from entry `from` of `upcoming()` to entry `to` (where it ends up).
    /// Songs move within their part: the ones you queued among themselves, the rest of the
    /// playlist among itself.
    pub fn move_upcoming(&mut self, from: usize, to: usize, id: &str) {
        let Some(from) = self.find_upcoming(from, id) else {
            return;
        };
        let queued = self.up_next.len();
        if from < queued {
            let to = to.min(queued - 1);
            if let Some(t) = self.up_next.remove(from) {
                self.up_next.insert(to, t);
            }
            return;
        }
        let rest = self.pos.map_or(0, |p| p + 1);
        let (a, b) = (rest + from - queued, rest + to.max(queued) - queued);
        if a < self.order.len() {
            let b = b.min(self.order.len() - 1);
            let i = self.order.remove(a);
            self.order.insert(b, i);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.current.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn tracks(n: usize) -> Vec<Track> {
        (0..n)
            .map(|i| Track {
                album_artist: String::new(),
                disc_no: None,
                id: format!("t{i}"),
                source: Source::Local,
                title: format!("T{i}"),
                artist: String::new(),
                album: String::new(),
                duration_ms: 0,
                track_no: None,
                art: None,
                uri: String::new(),
                added_at: 0,
            })
            .collect()
    }

    fn id(t: Option<Track>) -> String {
        t.map(|t| t.id).unwrap_or_default()
    }

    #[test]
    fn linear_playback_and_end() {
        let mut q = Queue::default();
        assert_eq!(id(q.set_context(tracks(3), 1, "ctx".into())), "t1");
        assert_eq!(id(q.peek_next().cloned()), "t2");
        assert_eq!(id(q.advance(false)), "t2");
        assert_eq!(q.advance(false), None);
    }

    #[test]
    fn repeat_modes() {
        let mut q = Queue::default();
        q.set_context(tracks(2), 1, String::new());
        q.repeat = RepeatMode::One;
        assert_eq!(id(q.advance(false)), "t1");
        // An explicit "next" leaves the repeated track (and wraps since repeat applies).
        assert_eq!(id(q.advance(true)), "t0");
        q.repeat = RepeatMode::All;
        assert_eq!(id(q.advance(false)), "t1");
        assert_eq!(id(q.advance(false)), "t0");
        assert_eq!(id(q.previous()), "t1");
    }

    #[test]
    fn up_next_goes_first() {
        let mut q = Queue::default();
        q.set_context(tracks(3), 0, String::new());
        let extra = tracks(5).split_off(3); // t3, t4
        q.enqueue(vec![extra[0].clone()]);
        q.play_next(vec![extra[1].clone()]);
        let upcoming: Vec<String> = q.upcoming(10).into_iter().map(|t| t.id).collect();
        assert_eq!(upcoming, vec!["t4", "t3", "t1", "t2"]);
        assert_eq!(id(q.advance(false)), "t4");
        assert_eq!(id(q.advance(false)), "t3");
        assert_eq!(id(q.advance(false)), "t1");
    }

    #[test]
    fn shuffle_keeps_current_first_and_covers_all() {
        let mut q = Queue {
            shuffle: true,
            ..Default::default()
        };
        q.set_context(tracks(20), 7, String::new());
        assert_eq!(id(q.current().cloned()), "t7");
        let mut seen: Vec<String> = q.upcoming(100).into_iter().map(|t| t.id).collect();
        seen.push("t7".into());
        seen.sort();
        let mut all: Vec<String> = tracks(20).into_iter().map(|t| t.id).collect();
        all.sort();
        assert_eq!(seen, all);
        q.set_shuffle(false);
        assert_eq!(id(q.peek_next().cloned()), "t8");
    }

    #[test]
    fn saved_queue_comes_back_the_same() {
        let all = tracks(10);
        let mut q = Queue {
            shuffle: true,
            repeat: RepeatMode::All,
            ..Default::default()
        };
        q.set_context(all[..6].to_vec(), 2, "Mix".into());
        q.advance(false);
        q.remove(0, &q.upcoming(1)[0].id.clone());
        q.enqueue(vec![all[9].clone()]);
        q.advance(false);

        let saved = serde_json::to_string(&q.state()).unwrap();
        let state: QueueState = serde_json::from_str(&saved).unwrap();
        let find = |id: &str| all.iter().find(|t| t.id == id).cloned();
        let mut back = Queue::default();
        assert!(back.restore(state.clone(), find));
        assert_eq!(back.state(), q.state());
        assert_eq!(id(back.current().cloned()), "t9");
        let ids = |q: &Queue| q.upcoming(20).into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(&back), ids(&q));

        // A song of the playlist that is gone: not restored (the caller falls back).
        let missing = |id: &str| (id != "t3").then(|| find(id)).flatten();
        assert!(!Queue::default().restore(state.clone(), missing));
        // Nonsense positions are refused rather than trusted.
        let mut bad = state;
        bad.order.push(99);
        assert!(!Queue::default().restore(bad, find));
    }

    #[test]
    fn moving_and_clearing_parts() {
        let mut q = Queue::default();
        q.set_context(tracks(6), 0, String::new());
        let extra = tracks(9);
        q.enqueue(vec![extra[7].clone(), extra[8].clone()]);
        let ids = |q: &Queue| q.upcoming(20).into_iter().map(|t| t.id).collect::<Vec<_>>();
        // Upcoming: t7 t8 (queued) | t1 t2 t3 t4 t5.
        q.move_upcoming(1, 0, "t8");
        assert_eq!(ids(&q), ["t8", "t7", "t1", "t2", "t3", "t4", "t5"]);
        q.move_upcoming(2, 5, "t1");
        assert_eq!(ids(&q), ["t8", "t7", "t2", "t3", "t4", "t1", "t5"]);
        // A queued song stays among the queued ones.
        q.move_upcoming(0, 6, "t8");
        assert_eq!(ids(&q), ["t7", "t8", "t2", "t3", "t4", "t1", "t5"]);
        q.clear_queued();
        assert_eq!(ids(&q), ["t2", "t3", "t4", "t1", "t5"]);
        q.enqueue(vec![extra[7].clone()]);
        q.clear_upcoming();
        assert_eq!(ids(&q), ["t7"]);
    }

    #[test]
    fn jump_and_remove() {
        let mut q = Queue::default();
        q.set_context(tracks(5), 0, String::new());
        q.remove(0, "t1");
        assert_eq!(id(q.jump_to(1, "t3")), "t3");
        assert_eq!(id(q.advance(false)), "t4");
    }

    #[test]
    fn jump_and_remove_find_the_song_shown() {
        let mut q = Queue::default();
        q.set_context(tracks(5), 0, String::new());
        // The list showed t2 at 1, but the queue has moved on one song since.
        q.advance(false);
        assert_eq!(id(q.jump_to(1, "t3")), "t3");
        q.remove(5, "t4");
        assert_eq!(q.upcoming(10).len(), 0);
        // A song that isn't upcoming anymore does nothing.
        assert!(q.jump_to(0, "t0").is_none());
    }

    #[test]
    fn removed_songs_stay_removed_with_shuffle() {
        let mut q = Queue::default();
        q.set_context(tracks(6), 0, String::new());
        q.remove(0, "t1");
        q.remove(0, "t2");
        q.set_shuffle(true);
        q.set_shuffle(false);
        let ids: Vec<String> = q.upcoming(10).into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["t3", "t4", "t5"]);
        q.clear_upcoming();
        q.set_shuffle(true);
        assert!(q.upcoming(10).is_empty());
    }

    #[test]
    fn previous_from_a_queued_song_goes_back_one() {
        let mut q = Queue::default();
        q.set_context(tracks(5), 2, String::new());
        q.enqueue(vec![tracks(9).remove(8)]);
        assert_eq!(id(q.advance(true)), "t8");
        assert_eq!(id(q.previous()), "t2");
        assert_eq!(id(q.advance(false)), "t3");
    }

    #[test]
    fn jumping_into_the_playlist_keeps_queued_songs() {
        let mut q = Queue::default();
        q.set_context(tracks(5), 0, String::new());
        q.enqueue(vec![tracks(9).remove(8)]);
        // Upcoming: t8 (queued), t1, t2, t3, t4.
        assert_eq!(id(q.jump_to(3, "t3")), "t3");
        assert_eq!(id(q.advance(false)), "t8");
        assert_eq!(id(q.advance(false)), "t4");
    }
}
