#[derive(Clone, Copy, Debug)]
pub struct EntryView<'a> {
    pub id: u32,
    pub frn: u64,
    pub parent_frn: u64,
    pub size: u64,
    pub mtime: u64,
    pub flags: u16,
    pub search_offset: u32,
    pub search_len: u16,
    pub display_offset: u32,
    pub display_len: u16,
    pub name_search: &'a str,
    pub name_display: &'a [u16],
}

impl EntryView<'_> {
    pub fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// The fields name filtering needs, read in one go. The hot search path uses
/// this instead of [`EntryView`] so it does not touch the display name or the
/// file reference number unless a query needs them.
#[derive(Clone, Copy, Debug)]
pub struct EntrySummary<'a> {
    pub name_search: &'a str,
    pub parent_frn: u64,
    pub size: u64,
    pub flags: u16,
}

impl EntrySummary<'_> {
    pub fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }
}
