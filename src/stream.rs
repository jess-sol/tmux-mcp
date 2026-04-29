/// Per-pane raw byte stream with position-indexed storage and a
/// reversible reader for consumption.
///
/// `RawStream` is the append-only buffer. It stores fully raw terminal
/// bytes (after tmux octal unescaping) with monotonic absolute offsets.
///
/// `StreamReader` is the cursor. It borrows the stream immutably and
/// provides forward reads, backward scans, seek/rewind, and peek
/// operations. Parsers receive a `StreamReader` and navigate through
/// the data however they need — a new parser on mode switch can reverse
/// through the buffer looking for an anchor (prompt pattern, OSC marker)
/// before starting to parse forward.
///
/// Uses a flat Vec<u8> with front-truncation on compaction. At our sizes
/// (256KB default) the memmove cost is negligible, and contiguous storage
/// means `&[u8]` returns are always valid.

const DEFAULT_CAPACITY: usize = 256 * 1024;

// --- RawStream (storage) ---

#[derive(Debug)]
pub struct RawStream {
    buf: Vec<u8>,
    /// Absolute offset of buf[0] from start of monitoring.
    /// Increases as bytes are evicted from the front.
    base_offset: u64,
    /// Bytes before this are safe to evict on the next compaction.
    safe_evict_offset: u64,
    /// Soft capacity limit. Buffer can temporarily exceed this.
    capacity: usize,
}

impl RawStream {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            base_offset: 0,
            safe_evict_offset: 0,
            capacity: DEFAULT_CAPACITY,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            base_offset: 0,
            safe_evict_offset: 0,
            capacity,
        }
    }

    /// Append raw bytes. Compacts if over capacity.
    pub fn append(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.capacity {
            self.compact();
        }
    }

    /// Absolute offset one past the last byte.
    pub fn head(&self) -> u64 {
        self.base_offset + self.buf.len() as u64
    }

    /// Absolute offset of the oldest available byte.
    pub fn tail(&self) -> u64 {
        self.base_offset
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Total bytes ever written (head offset, including evicted).
    pub fn total_written(&self) -> u64 {
        self.head()
    }

    /// Mark a safe eviction point. Only advances, never retreats.
    pub fn mark_safe_boundary(&mut self, offset: u64) {
        if offset > self.safe_evict_offset {
            self.safe_evict_offset = offset;
        }
    }

    /// Create a reader positioned at the given offset.
    /// Clamps to [tail, head].
    pub fn reader(&self, cursor: u64) -> StreamReader<'_> {
        let clamped = cursor.clamp(self.tail(), self.head());
        StreamReader { stream: self, cursor: clamped }
    }

    /// Reader starting at the oldest available byte.
    pub fn reader_at_tail(&self) -> StreamReader<'_> {
        self.reader(self.tail())
    }

    /// Reader starting at the newest byte (at end, nothing to read forward).
    pub fn reader_at_head(&self) -> StreamReader<'_> {
        self.reader(self.head())
    }

    // --- Internal ---

    /// Read bytes from absolute offset to head. Empty if out of range.
    fn read_from(&self, offset: u64) -> &[u8] {
        if offset < self.base_offset || offset >= self.head() {
            return &[];
        }
        let start = (offset - self.base_offset) as usize;
        &self.buf[start..]
    }

    /// Read bytes from absolute offset `from` up to (not including) `to`.
    fn read_range(&self, from: u64, to: u64) -> &[u8] {
        let from = from.max(self.base_offset);
        let to = to.min(self.head());
        if from >= to {
            return &[];
        }
        let start = (from - self.base_offset) as usize;
        let end = (to - self.base_offset) as usize;
        &self.buf[start..end]
    }

    fn compact(&mut self) {
        let evict_to = self.safe_evict_offset;
        if evict_to <= self.base_offset {
            return;
        }
        let drain_count = ((evict_to - self.base_offset) as usize).min(self.buf.len());
        if drain_count == 0 {
            return;
        }
        self.buf.drain(..drain_count);
        self.base_offset += drain_count as u64;
    }
}

impl Default for RawStream {
    fn default() -> Self {
        Self::new()
    }
}

// --- StreamReader (cursor) ---

/// Reversible reader over a `RawStream`.
///
/// Borrows the stream immutably for its lifetime. The cursor is an
/// absolute byte offset that can move in either direction within the
/// stream's available range [tail, head].
///
/// Designed for parsers: peek at bytes without advancing, advance by
/// exactly the number consumed, reverse to scan for anchors on mode
/// switch, seek to arbitrary positions for replay.
#[derive(Debug)]
pub struct StreamReader<'a> {
    stream: &'a RawStream,
    cursor: u64,
}

impl<'a> StreamReader<'a> {
    // --- Position ---

    /// Current absolute cursor position.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Absolute offset of the oldest available byte.
    pub fn tail(&self) -> u64 {
        self.stream.tail()
    }

    /// Absolute offset one past the newest byte.
    pub fn head(&self) -> u64 {
        self.stream.head()
    }

    /// Bytes available forward from cursor to head.
    pub fn remaining(&self) -> usize {
        self.head().saturating_sub(self.cursor) as usize
    }

    /// Bytes available backward from cursor to tail.
    pub fn preceding(&self) -> usize {
        self.cursor.saturating_sub(self.tail()) as usize
    }

    /// Whether the cursor is at the head (nothing to read forward).
    pub fn at_end(&self) -> bool {
        self.cursor >= self.head()
    }

    /// Whether the cursor is at the tail (nothing to read backward).
    pub fn at_start(&self) -> bool {
        self.cursor <= self.tail()
    }

    // --- Forward operations ---

    /// Look at up to `max_len` bytes ahead without moving the cursor.
    pub fn peek(&self, max_len: usize) -> &'a [u8] {
        let all = self.stream.read_from(self.cursor);
        &all[..all.len().min(max_len)]
    }

    /// Look at all remaining bytes without moving the cursor.
    pub fn peek_all(&self) -> &'a [u8] {
        self.stream.read_from(self.cursor)
    }

    /// Move cursor forward by `n` bytes. Clamps to head.
    pub fn advance(&mut self, n: usize) {
        self.cursor = (self.cursor + n as u64).min(self.head());
    }

    /// Read up to `max_len` bytes and advance the cursor past them.
    pub fn read(&mut self, max_len: usize) -> &'a [u8] {
        let data = self.peek(max_len);
        self.cursor += data.len() as u64;
        data
    }

    /// Read all remaining bytes and advance to head.
    pub fn read_all(&mut self) -> &'a [u8] {
        let data = self.peek_all();
        self.cursor = self.head();
        data
    }

    // --- Backward operations ---

    /// Look at up to `max_len` bytes behind the cursor without moving it.
    /// Returns bytes in forward order (oldest first).
    pub fn peek_back(&self, max_len: usize) -> &'a [u8] {
        let avail = self.preceding();
        let n = avail.min(max_len);
        let from = self.cursor - n as u64;
        self.stream.read_range(from, self.cursor)
    }

    /// Move cursor backward by `n` bytes. Clamps to tail.
    pub fn rewind(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n as u64).max(self.tail());
    }

    /// Read up to `max_len` bytes behind the cursor and move the cursor
    /// back past them. Returns bytes in forward order.
    pub fn read_back(&mut self, max_len: usize) -> &'a [u8] {
        let data = self.peek_back(max_len);
        self.cursor -= data.len() as u64;
        data
    }

    // --- Seek ---

    /// Move cursor to an absolute offset. Clamps to [tail, head].
    pub fn seek(&mut self, offset: u64) {
        self.cursor = offset.clamp(self.tail(), self.head());
    }

    /// Move cursor to the tail (oldest available byte).
    pub fn seek_to_tail(&mut self) {
        self.cursor = self.tail();
    }

    /// Move cursor to the head (one past newest byte).
    pub fn seek_to_head(&mut self) {
        self.cursor = self.head();
    }

    // --- Bulk access ---

    /// Read a range [from, to) without moving the cursor.
    pub fn slice(&self, from: u64, to: u64) -> &'a [u8] {
        self.stream.read_range(from, to)
    }

    /// Scan backward from cursor for a byte sequence.
    /// Returns the absolute offset of the match start, or None.
    /// Does NOT move the cursor.
    pub fn rfind(&self, needle: &[u8]) -> Option<u64> {
        if needle.is_empty() {
            return Some(self.cursor);
        }
        let data = self.stream.read_range(self.tail(), self.cursor);
        // Scan backward through the data for the needle
        if data.len() < needle.len() {
            return None;
        }
        for i in (0..=(data.len() - needle.len())).rev() {
            if data[i..i + needle.len()] == *needle {
                return Some(self.tail() + i as u64);
            }
        }
        None
    }

    /// Scan forward from cursor for a byte sequence.
    /// Returns the absolute offset of the match start, or None.
    /// Does NOT move the cursor.
    pub fn find(&self, needle: &[u8]) -> Option<u64> {
        if needle.is_empty() {
            return Some(self.cursor);
        }
        let data = self.stream.read_from(self.cursor);
        if data.len() < needle.len() {
            return None;
        }
        for i in 0..=(data.len() - needle.len()) {
            if data[i..i + needle.len()] == *needle {
                return Some(self.cursor + i as u64);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- RawStream basics ---

    #[test]
    fn new_stream_is_empty() {
        let s = RawStream::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.head(), 0);
        assert_eq!(s.tail(), 0);
    }

    #[test]
    fn append_updates_head() {
        let mut s = RawStream::new();
        s.append(b"hello");
        assert_eq!(s.len(), 5);
        assert_eq!(s.head(), 5);
        assert_eq!(s.tail(), 0);
    }

    #[test]
    fn append_empty_is_noop() {
        let mut s = RawStream::new();
        s.append(b"");
        assert!(s.is_empty());
    }

    #[test]
    fn multiple_appends() {
        let mut s = RawStream::new();
        s.append(b"aaa");
        s.append(b"bbb");
        s.append(b"ccc");
        assert_eq!(s.len(), 9);
        assert_eq!(s.head(), 9);
    }

    #[test]
    fn safe_boundary_only_advances() {
        let mut s = RawStream::new();
        s.append(b"hello");
        s.mark_safe_boundary(3);
        s.mark_safe_boundary(1); // ignored
        assert_eq!(s.safe_evict_offset, 3);
    }

    #[test]
    fn compaction_on_overflow() {
        let mut s = RawStream::with_capacity(10);
        s.append(b"01234");
        s.mark_safe_boundary(3);
        s.append(b"56789ab"); // 12 > 10, compact evicts 0..3
        assert_eq!(s.tail(), 3);
        assert_eq!(s.head(), 12);
    }

    #[test]
    fn no_compaction_without_safe_boundary() {
        let mut s = RawStream::with_capacity(10);
        s.append(b"0123456789abcdef"); // 16 > 10, but safe_evict=0
        assert_eq!(s.tail(), 0);
        assert_eq!(s.len(), 16);
    }

    #[test]
    fn sequential_compactions() {
        let mut s = RawStream::with_capacity(8);
        s.append(b"aaaa");
        s.mark_safe_boundary(2);
        s.append(b"bbbbbb"); // 10>8 → evict 0..2
        assert_eq!(s.tail(), 2);

        s.mark_safe_boundary(6);
        s.append(b"cc"); // 10>8 → evict 2..6
        assert_eq!(s.tail(), 6);
    }

    #[test]
    fn total_written_tracks_all_bytes() {
        let mut s = RawStream::with_capacity(10);
        s.append(b"12345");
        s.mark_safe_boundary(5);
        s.append(b"67890abcde");
        assert_eq!(s.total_written(), 15);
    }

    // --- StreamReader: forward operations ---

    #[test]
    fn reader_peek_and_advance() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let mut r = s.reader(0);
        assert_eq!(r.peek(3), b"abc");
        assert_eq!(r.cursor(), 0); // peek doesn't move
        r.advance(3);
        assert_eq!(r.cursor(), 3);
        assert_eq!(r.peek(3), b"def");
    }

    #[test]
    fn reader_read_advances() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let mut r = s.reader(0);
        assert_eq!(r.read(3), b"abc");
        assert_eq!(r.cursor(), 3);
        assert_eq!(r.read(3), b"def");
        assert_eq!(r.cursor(), 6);
        assert!(r.at_end());
    }

    #[test]
    fn reader_read_all() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let mut r = s.reader(2);
        assert_eq!(r.read_all(), b"llo");
        assert!(r.at_end());
    }

    #[test]
    fn reader_peek_all() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let r = s.reader(3);
        assert_eq!(r.peek_all(), b"def");
        assert_eq!(r.cursor(), 3); // unchanged
    }

    #[test]
    fn reader_remaining_and_preceding() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let r = s.reader(2);
        assert_eq!(r.remaining(), 4);
        assert_eq!(r.preceding(), 2);
    }

    #[test]
    fn reader_at_end_and_at_start() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let r = s.reader_at_tail();
        assert!(r.at_start());
        assert!(!r.at_end());

        let r = s.reader_at_head();
        assert!(r.at_end());
        assert!(!r.at_start());
    }

    #[test]
    fn reader_advance_clamps_to_head() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let mut r = s.reader(3);
        r.advance(100);
        assert_eq!(r.cursor(), 5);
        assert!(r.at_end());
    }

    #[test]
    fn reader_read_past_end() {
        let mut s = RawStream::new();
        s.append(b"ab");

        let mut r = s.reader(0);
        assert_eq!(r.read(10), b"ab");
        assert_eq!(r.read(10), b"");
    }

    // --- StreamReader: backward operations ---

    #[test]
    fn reader_peek_back() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let r = s.reader(4);
        assert_eq!(r.peek_back(2), b"cd");
        assert_eq!(r.cursor(), 4); // unchanged
    }

    #[test]
    fn reader_peek_back_all_preceding() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let r = s.reader(4);
        assert_eq!(r.peek_back(100), b"abcd");
    }

    #[test]
    fn reader_rewind() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let mut r = s.reader(4);
        r.rewind(2);
        assert_eq!(r.cursor(), 2);
        assert_eq!(r.peek(4), b"cdef");
    }

    #[test]
    fn reader_rewind_clamps_to_tail() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let mut r = s.reader(2);
        r.rewind(100);
        assert_eq!(r.cursor(), 0);
        assert!(r.at_start());
    }

    #[test]
    fn reader_read_back() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let mut r = s.reader(4);
        assert_eq!(r.read_back(2), b"cd");
        assert_eq!(r.cursor(), 2);
        assert_eq!(r.read_back(2), b"ab");
        assert_eq!(r.cursor(), 0);
    }

    #[test]
    fn reader_read_back_at_start() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let mut r = s.reader_at_tail();
        assert_eq!(r.read_back(5), b"");
        assert_eq!(r.cursor(), 0);
    }

    // --- StreamReader: seek ---

    #[test]
    fn reader_seek_absolute() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let mut r = s.reader(0);
        r.seek(3);
        assert_eq!(r.cursor(), 3);
        assert_eq!(r.peek(3), b"def");
    }

    #[test]
    fn reader_seek_clamps_to_range() {
        let mut s = RawStream::with_capacity(10);
        s.append(b"hello");
        s.mark_safe_boundary(2);
        s.append(b"worldworld"); // compact evicts 0..2
        // tail=2, head=15

        let mut r = s.reader(5);
        r.seek(0); // before tail → clamp to tail
        assert_eq!(r.cursor(), 2);

        r.seek(1000); // past head → clamp to head
        assert_eq!(r.cursor(), 15);
    }

    #[test]
    fn reader_seek_to_tail_and_head() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let mut r = s.reader(3);
        r.seek_to_tail();
        assert_eq!(r.cursor(), 0);

        r.seek_to_head();
        assert_eq!(r.cursor(), 6);
    }

    // --- StreamReader: slice ---

    #[test]
    fn reader_slice() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let r = s.reader(0);
        assert_eq!(r.slice(1, 4), b"bcd");
    }

    #[test]
    fn reader_slice_clamps_to_bounds() {
        let mut s = RawStream::new();
        s.append(b"abcdef");

        let r = s.reader(0);
        assert_eq!(r.slice(0, 100), b"abcdef"); // to > head → clamp
        assert_eq!(r.slice(4, 2), b""); // from > to → empty
    }

    // --- StreamReader: find / rfind ---

    #[test]
    fn reader_find_forward() {
        let mut s = RawStream::new();
        s.append(b"hello world hello");

        let r = s.reader(0);
        assert_eq!(r.find(b"world"), Some(6));
        assert_eq!(r.cursor(), 0); // find doesn't move cursor
    }

    #[test]
    fn reader_find_from_cursor() {
        let mut s = RawStream::new();
        s.append(b"aaa bbb aaa");

        let r = s.reader(4);
        // Search starts from cursor position
        assert_eq!(r.find(b"aaa"), Some(8));
    }

    #[test]
    fn reader_find_not_found() {
        let mut s = RawStream::new();
        s.append(b"hello world");

        let r = s.reader(0);
        assert_eq!(r.find(b"xyz"), None);
    }

    #[test]
    fn reader_find_empty_needle() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let r = s.reader(3);
        assert_eq!(r.find(b""), Some(3));
    }

    #[test]
    fn reader_rfind_backward() {
        let mut s = RawStream::new();
        s.append(b"aaa bbb aaa bbb");

        let r = s.reader(15); // at head
        assert_eq!(r.rfind(b"aaa"), Some(8)); // finds last occurrence before cursor
    }

    #[test]
    fn reader_rfind_from_middle() {
        let mut s = RawStream::new();
        s.append(b"aaa bbb aaa bbb");

        let r = s.reader(7); // between the two "aaa"
        assert_eq!(r.rfind(b"aaa"), Some(0)); // only first one is before cursor
    }

    #[test]
    fn reader_rfind_not_found() {
        let mut s = RawStream::new();
        s.append(b"hello world");

        let r = s.reader(11);
        assert_eq!(r.rfind(b"xyz"), None);
    }

    #[test]
    fn reader_rfind_empty_needle() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let r = s.reader(3);
        assert_eq!(r.rfind(b""), Some(3));
    }

    #[test]
    fn reader_rfind_at_start() {
        let mut s = RawStream::new();
        s.append(b"hello");

        let r = s.reader(0);
        assert_eq!(r.rfind(b"hello"), None); // cursor at 0, nothing before
    }

    // --- StreamReader: after compaction ---

    #[test]
    fn reader_after_compaction() {
        let mut s = RawStream::with_capacity(10);
        s.append(b"aaaa"); // 0..4
        s.append(b"bbbb"); // 4..8
        s.mark_safe_boundary(4);
        s.append(b"cccccccc"); // 8..16, triggers compaction of 0..4

        let mut r = s.reader_at_tail();
        assert_eq!(r.cursor(), 4);
        assert_eq!(r.read(4), b"bbbb");
        assert_eq!(r.read(8), b"cccccccc");
        assert!(r.at_end());
    }

    #[test]
    fn reader_cursor_clamped_after_eviction() {
        // If a cursor offset was evicted, creating a reader clamps to tail
        let mut s = RawStream::with_capacity(10);
        s.append(b"01234");
        s.mark_safe_boundary(4);
        s.append(b"56789abcde"); // compact evicts 0..4

        let r = s.reader(2); // offset 2 was evicted → clamped to tail=4
        assert_eq!(r.cursor(), 4);
    }

    // --- StreamReader: mode-switch reverse scan ---

    #[test]
    fn reverse_scan_for_prompt_anchor() {
        // Simulate: pane was in Raw mode, accumulated NOS output.
        // Mode switches to Prompt. New parser reverses looking for
        // "Router#" to find where to start parsing.
        let mut s = RawStream::new();
        s.append(b"some earlier output\n");
        s.append(b"Router#show interfaces\n");
        s.append(b"GigabitEthernet0/0 is up\n");
        s.append(b"  Internet address is 10.0.0.1/24\n");
        s.append(b"Router#");

        let mut r = s.reader_at_head();
        let prompt_offset = r.rfind(b"Router#").unwrap();
        r.seek(prompt_offset);
        assert_eq!(r.peek(7), b"Router#");
        // Parser would now start parsing forward from here
        assert_eq!(r.remaining(), 7); // at the last prompt
    }

    #[test]
    fn reverse_scan_finds_second_to_last_prompt() {
        // For structured parsing, we want the prompt BEFORE the last one
        // (the last prompt is the current idle state, the one before starts
        // the most recent command).
        let mut s = RawStream::new();
        s.append(b"Router#show version\nCisco IOS v15.2\nRouter#");

        let mut r = s.reader_at_head();
        // Find the last "Router#"
        let last = r.rfind(b"Router#").unwrap();
        // Now find the one before that
        r.seek(last); // cursor at last Router#
        r.rewind(1); // back up past the # so rfind doesn't re-find it
        let prev = r.rfind(b"Router#");
        assert_eq!(prev, Some(0));
    }

    #[test]
    fn forward_read_after_reverse_anchor() {
        let mut s = RawStream::new();
        s.append(b"junk\nRouter>enable\nRouter#conf t\nRouter(config)#");

        let mut r = s.reader_at_head();
        // Find the most recent "Router" occurrence
        let anchor = r.rfind(b"Router").unwrap();
        r.seek(anchor);
        // Read forward to see what's there
        let rest = r.peek_all();
        assert_eq!(rest, b"Router(config)#");
    }

    // --- Stress ---

    #[test]
    fn large_append_stress() {
        let mut s = RawStream::with_capacity(1024);
        for i in 0u64..100 {
            let chunk: Vec<u8> = (0..100).map(|j| ((i * 100 + j) % 256) as u8).collect();
            s.append(&chunk);
            if i > 0 {
                s.mark_safe_boundary(i * 100);
            }
        }
        assert_eq!(s.head(), 10_000);
        assert!(s.tail() > 0);
        let r = s.reader_at_tail();
        assert_eq!(r.remaining() as u64, s.head() - s.tail());
    }

    #[test]
    fn reader_interleaved_forward_backward() {
        let mut s = RawStream::new();
        s.append(b"abcdefghij");

        let mut r = s.reader(0);
        assert_eq!(r.read(3), b"abc"); // cursor=3
        assert_eq!(r.read(2), b"de");  // cursor=5
        r.rewind(4);                    // cursor=1
        assert_eq!(r.peek(3), b"bcd");
        r.advance(3);                   // cursor=4
        assert_eq!(r.read_back(2), b"cd"); // cursor=2
        assert_eq!(r.read(2), b"cd");  // cursor=4
    }
}
