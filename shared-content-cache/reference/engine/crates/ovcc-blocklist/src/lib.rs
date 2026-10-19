// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Azure Put Block List parsing and pure block-commit planning.
//!
//! An Azure block blob is uploaded as many `Put Block` requests, each staging a
//! base64-identified fragment, and then finalized by a `Put Block List` commit
//! whose XML body names the fragments in their final order. This crate turns
//! that XML into an ordered [`BlockId`] list with [`parse_block_list`] and
//! prevalidates the complete list with [`plan_segments`].
//!
//! Planning reads only exact staged-block metadata through [`BlockMetadataSource`].
//! It preserves every list occurrence while identifying unique immutable sources
//! for a later storage handoff. The crate does no filesystem or network I/O, is
//! not async, and never reads, writes, or copies fragment bytes.

use std::collections::HashMap;

/// An Azure block identifier, the base64 token naming one staged fragment.
///
/// The value is the decoded id as it appears in the commit XML. Base64 ids never
/// contain `<`, which [`parse_block_list`] relies on to find leaf tags.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(String);

impl BlockId {
    /// Wraps an already-decoded block id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrows the id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `tag` is a leaf tag that names a committed fragment. A commit lists
/// blocks in document order across `<Latest>`, `<Committed>`, and
/// `<Uncommitted>`; the tag selects which staged copy Azure uses but does not
/// affect the order captured here.
fn is_block_tag(tag: &str) -> bool {
    tag.eq_ignore_ascii_case("latest")
        || tag.eq_ignore_ascii_case("committed")
        || tag.eq_ignore_ascii_case("uncommitted")
}

/// Extracts the ordered block ids from a Put Block List XML body.
///
/// Returns the trimmed text of each leaf block tag in document order. Only a leaf
/// whose text holds no `<` and is closed by its matching end tag counts, so the
/// `<BlockList>` wrapper and the `<?xml?>` declaration are skipped. Non-UTF-8
/// input yields an empty list.
pub fn parse_block_list(xml: &[u8]) -> Vec<BlockId> {
    let Ok(s) = std::str::from_utf8(xml) else {
        return Vec::new();
    };
    let bytes = s.as_bytes();
    let mut ids = Vec::new();
    let mut i = 0;
    while let Some(open) = find_from(bytes, i, b'<') {
        // A tag is an alphabetic name immediately closed by '>'; anything else
        // (the `<?xml?>` declaration, attributes) is not a block leaf.
        let name_start = open + 1;
        let mut name_end = name_start;
        while name_end < bytes.len() && bytes[name_end].is_ascii_alphabetic() {
            name_end += 1;
        }
        if name_end == name_start || name_end >= bytes.len() || bytes[name_end] != b'>' {
            i = open + 1;
            continue;
        }

        let tag = &s[name_start..name_end];
        let text_start = name_end + 1;
        let text_end = find_from(bytes, text_start, b'<').unwrap_or(bytes.len());
        let close = format!("</{tag}>");
        if s[text_end..].starts_with(&close) {
            if is_block_tag(tag) {
                ids.push(BlockId(s[text_start..text_end].trim().to_owned()));
            }
            i = text_end + close.len();
        } else {
            // A non-leaf element such as the wrapper; resume scanning inside it
            // so its child leaves are still found.
            i = text_start;
        }
    }
    ids
}

/// The offset of the next `needle` at or after `start`, if any.
fn find_from(haystack: &[u8], start: usize, needle: u8) -> Option<usize> {
    if start >= haystack.len() {
        return None;
    }
    haystack[start..]
        .iter()
        .position(|&b| b == needle)
        .map(|p| p + start)
}

/// Immutable metadata needed to plan one staged block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMetadata {
    length: u64,
}

impl BlockMetadata {
    /// Creates metadata with the staged block's exact byte length.
    pub const fn new(length: u64) -> Self {
        Self { length }
    }

    /// Returns the staged block's exact byte length.
    pub const fn length(self) -> u64 {
        self.length
    }
}

/// Supplies immutable metadata for staged block ids.
///
/// A return value of `None` means the id is not eligible for commit. Metadata
/// must remain stable for the duration of one [`plan_segments`] call.
pub trait BlockMetadataSource {
    /// Returns exact metadata for an eligible staged block.
    fn metadata(&self, id: &BlockId) -> Option<BlockMetadata>;
}

/// One unique staged source needed by a segment plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedSource {
    block_id: BlockId,
    metadata: BlockMetadata,
}

impl PlannedSource {
    /// Returns the staged block id.
    pub fn block_id(&self) -> &BlockId {
        &self.block_id
    }

    /// Returns the source metadata captured during planning.
    pub const fn metadata(&self) -> BlockMetadata {
        self.metadata
    }

    /// Returns the source's exact byte length.
    pub const fn length(&self) -> u64 {
        self.metadata.length()
    }
}

/// One ordered occurrence from the committed block list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedSegment {
    block_id: BlockId,
    metadata: BlockMetadata,
    source_index: usize,
}

impl PlannedSegment {
    /// Returns the block id named by this list occurrence.
    pub fn block_id(&self) -> &BlockId {
        &self.block_id
    }

    /// Returns the metadata captured for this occurrence.
    pub const fn metadata(&self) -> BlockMetadata {
        self.metadata
    }

    /// Returns this segment's exact byte length.
    pub const fn length(&self) -> u64 {
        self.metadata.length()
    }

    /// Returns the index of this block in [`SegmentPlan::unique_sources`].
    pub const fn source_index(&self) -> usize {
        self.source_index
    }
}

/// A fully validated ordered block-list plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentPlan {
    segments: Vec<PlannedSegment>,
    unique_sources: Vec<PlannedSource>,
    total_length: u64,
}

impl SegmentPlan {
    /// Returns every committed list occurrence in exact document order.
    pub fn segments(&self) -> &[PlannedSegment] {
        &self.segments
    }

    /// Returns staged sources once each, in first-occurrence order.
    pub fn unique_sources(&self) -> &[PlannedSource] {
        &self.unique_sources
    }

    /// Returns the checked sum of every segment occurrence.
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }
}

/// Why an ordered block list could not be planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanningError {
    /// The commit list named no blocks.
    EmptyList,
    /// The first committed id without eligible staged metadata.
    MissingBlock(BlockId),
    /// The sum of all ordered segment occurrences exceeds `u64::MAX`.
    TotalLengthOverflow,
}

impl std::fmt::Display for PlanningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyList => write!(f, "block list named no blocks"),
            Self::MissingBlock(id) => write!(f, "committed block {id} is not staged"),
            Self::TotalLengthOverflow => {
                write!(f, "block list total length exceeds u64::MAX")
            }
        }
    }
}

impl std::error::Error for PlanningError {}

/// Carries the planning failure into the shared error type without losing its
/// rendered detail, so a caller working in [`ovcc_core::Result`] can use `?`.
impl From<PlanningError> for ovcc_core::Error {
    fn from(error: PlanningError) -> Self {
        Self::storage(error.to_string())
    }
}

/// Prevalidates an ordered block list and returns its metadata-only segment plan.
///
/// Metadata is queried once per unique id and reused for duplicate occurrences.
/// Every id is resolved before the checked total is computed, so a missing id is
/// reported in list order even when the available lengths would overflow. The
/// returned unique source list is deterministic first-occurrence order.
pub fn plan_segments<S>(list: &[BlockId], source: &S) -> Result<SegmentPlan, PlanningError>
where
    S: BlockMetadataSource + ?Sized,
{
    if list.is_empty() {
        return Err(PlanningError::EmptyList);
    }

    let mut metadata_by_id = HashMap::<&str, (BlockMetadata, usize)>::with_capacity(list.len());
    let mut resolved = Vec::with_capacity(list.len());
    let mut unique_sources = Vec::with_capacity(list.len());

    for id in list {
        let (metadata, source_index) =
            if let Some(cached) = metadata_by_id.get(id.as_str()).copied() {
                cached
            } else {
                let metadata = source
                    .metadata(id)
                    .ok_or_else(|| PlanningError::MissingBlock(id.clone()))?;
                let source_index = unique_sources.len();
                metadata_by_id.insert(id.as_str(), (metadata, source_index));
                unique_sources.push(PlannedSource {
                    block_id: id.clone(),
                    metadata,
                });
                (metadata, source_index)
            };
        resolved.push((metadata, source_index));
    }

    let mut total_length = 0u64;
    let mut segments = Vec::with_capacity(list.len());
    for (id, (metadata, source_index)) in list.iter().zip(resolved) {
        total_length = total_length
            .checked_add(metadata.length())
            .ok_or(PlanningError::TotalLengthOverflow)?;
        segments.push(PlannedSegment {
            block_id: id.clone(),
            metadata,
            source_index,
        });
    }

    Ok(SegmentPlan {
        segments,
        unique_sources,
        total_length,
    })
}

#[cfg(test)]
mod state_machine_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    // The lookup log proves duplicate occurrences reuse their first metadata
    // result instead of querying an immutable staged source again.
    struct MemMetadata {
        lengths: HashMap<String, u64>,
        lookup_counts: RefCell<HashMap<String, usize>>,
    }

    impl MemMetadata {
        fn from(entries: &[(&str, u64)]) -> Self {
            Self::owned(
                entries
                    .iter()
                    .map(|(id, length)| ((*id).to_owned(), *length)),
            )
        }

        fn owned(entries: impl IntoIterator<Item = (String, u64)>) -> Self {
            Self {
                lengths: entries.into_iter().collect(),
                lookup_counts: RefCell::new(HashMap::new()),
            }
        }

        fn lookup_count(&self, id: &str) -> usize {
            self.lookup_counts.borrow().get(id).copied().unwrap_or(0)
        }
    }

    impl BlockMetadataSource for MemMetadata {
        fn metadata(&self, id: &BlockId) -> Option<BlockMetadata> {
            *self
                .lookup_counts
                .borrow_mut()
                .entry(id.as_str().to_owned())
                .or_default() += 1;
            self.lengths
                .get(id.as_str())
                .copied()
                .map(BlockMetadata::new)
        }
    }

    fn ids(v: &[&str]) -> Vec<BlockId> {
        v.iter().map(|s| BlockId::new(*s)).collect()
    }

    fn planned_ids(segments: &[PlannedSegment]) -> Vec<&str> {
        segments
            .iter()
            .map(|segment| segment.block_id().as_str())
            .collect()
    }

    fn source_ids(sources: &[PlannedSource]) -> Vec<&str> {
        sources
            .iter()
            .map(|source| source.block_id().as_str())
            .collect()
    }

    // Structural parsing: leaf-id extraction, tag and case handling, and
    // malformed or non-block input.

    #[test]
    fn parse_empty_blank_and_wrapper_only_are_empty() {
        assert!(parse_block_list(b"").is_empty());
        assert!(parse_block_list(b"   ").is_empty());
        assert!(parse_block_list(b"<BlockList></BlockList>").is_empty());
    }

    #[test]
    fn parse_non_utf8_is_empty() {
        assert!(parse_block_list(&[0xff, 0xfe, 0x00, b'<']).is_empty());
    }

    #[test]
    fn parse_latest_in_document_order() {
        assert_eq!(
            parse_block_list(b"<BlockList><Latest>QQ==</Latest></BlockList>"),
            ids(&["QQ=="])
        );
        let xml = b"<?xml version=\"1.0\"?><BlockList>\
            <Latest>QQ==</Latest><Latest>Ug==</Latest><Latest>Uw==</Latest></BlockList>";
        assert_eq!(parse_block_list(xml), ids(&["QQ==", "Ug==", "Uw=="]));
    }

    #[test]
    fn parse_committed_and_uncommitted_leaves() {
        let xml =
            b"<BlockList><Committed>YQ==</Committed><Uncommitted>Yg==</Uncommitted></BlockList>";
        assert_eq!(parse_block_list(xml), ids(&["YQ==", "Yg=="]));
    }

    #[test]
    fn parse_tag_name_is_case_insensitive() {
        assert_eq!(
            parse_block_list(b"<BlockList><LATEST>QQ==</LATEST><latest>Ug==</latest></BlockList>"),
            ids(&["QQ==", "Ug=="])
        );
    }

    #[test]
    fn parse_open_close_case_mismatch_is_skipped() {
        // The close tag must match the open tag's case, so a mismatched pair is
        // not a well-formed leaf and yields no id.
        assert!(parse_block_list(b"<BlockList><Latest>QQ==</latest></BlockList>").is_empty());
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        assert_eq!(
            parse_block_list(b"<BlockList><Latest>  QQ==  </Latest></BlockList>"),
            ids(&["QQ=="])
        );
    }

    #[test]
    fn parse_declaration_and_unknown_tag_ignored() {
        let xml = b"<?xml version=\"1.0\"?><BlockList>\
            <Foo>ignored</Foo><Latest>QQ==</Latest></BlockList>";
        assert_eq!(parse_block_list(xml), ids(&["QQ=="]));
    }

    #[test]
    fn parse_empty_id_leaf_yields_one_empty_id() {
        // A degenerate `<Latest></Latest>` produces a single empty id; a later
        // plan of it fails as a missing block rather than publishing junk.
        assert_eq!(
            parse_block_list(b"<BlockList><Latest></Latest></BlockList>"),
            vec![BlockId::new("")]
        );
    }

    #[test]
    fn parse_unclosed_tag_is_skipped() {
        assert!(parse_block_list(b"<BlockList><Latest>abc").is_empty());
    }

    // The exact wire shape @azure/storage-blob emits for a GitHub Actions
    // Artifacts v4 upload; the three ids differ only in their trailing base64
    // char. Planning must retain this exact order for segmented publication.
    #[test]
    fn parse_golden_artifacts_v4_block_list() {
        let xml = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><BlockList>",
            "<Latest>ZGE5N2I5MzctODljNi00YzAwLWIzYWQtODhhNDAzMjI4NmU4MDAwMDAwMDAwMDAw</Latest>",
            "<Latest>ZGE5N2I5MzctODljNi00YzAwLWIzYWQtODhhNDAzMjI4NmU4MDAwMDAwMDAwMDAx</Latest>",
            "<Latest>ZGE5N2I5MzctODljNi00YzAwLWIzYWQtODhhNDAzMjI4NmU4MDAwMDAwMDAwMDAy</Latest>",
            "</BlockList>",
        );
        let out = parse_block_list(xml.as_bytes());
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].as_str(),
            "ZGE5N2I5MzctODljNi00YzAwLWIzYWQtODhhNDAzMjI4NmU4MDAwMDAwMDAwMDAw"
        );
        assert_eq!(out[2].as_str().chars().last(), Some('y'));
    }

    #[test]
    fn plan_preserves_list_order_and_metadata() {
        // Arrange.
        let source = MemMetadata::from(&[("a", 1), ("b", 2), ("c", 3)]);
        let list = ids(&["c", "a", "b"]);

        // Act.
        let plan = plan_segments(&list, &source).expect("all listed blocks are staged");

        // Assert.
        assert_eq!(planned_ids(plan.segments()), vec!["c", "a", "b"]);
        assert_eq!(
            plan.segments()
                .iter()
                .map(PlannedSegment::length)
                .collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
        assert_eq!(source_ids(plan.unique_sources()), vec!["c", "a", "b"]);
        assert_eq!(
            plan.segments()
                .iter()
                .map(PlannedSegment::source_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn plan_reuses_metadata_for_duplicate_occurrences() {
        // Arrange.
        let source = MemMetadata::from(&[("a", 3), ("b", 5)]);
        let list = ids(&["a", "b", "a", "a"]);

        // Act.
        let plan = plan_segments(&list, &source).expect("all listed blocks are staged");

        // Assert.
        assert_eq!(planned_ids(plan.segments()), vec!["a", "b", "a", "a"]);
        assert_eq!(
            plan.segments()
                .iter()
                .map(PlannedSegment::length)
                .collect::<Vec<_>>(),
            vec![3, 5, 3, 3]
        );
        assert_eq!(
            plan.segments()
                .iter()
                .map(PlannedSegment::source_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 0, 0]
        );
        assert_eq!(source_ids(plan.unique_sources()), vec!["a", "b"]);
        assert_eq!(source.lookup_count("a"), 1);
        assert_eq!(source.lookup_count("b"), 1);
        assert_eq!(plan.total_length(), 14);
    }

    #[test]
    fn plan_accepts_empty_fragment_at_every_position() {
        // Arrange.
        let source = MemMetadata::from(&[("empty", 0), ("x", 5), ("y", 7)]);
        let cases = [
            (["empty", "x", "y"], 0usize),
            (["x", "empty", "y"], 1usize),
            (["x", "y", "empty"], 2usize),
        ];

        for (ids_in_order, empty_position) in cases {
            // Act.
            let plan = plan_segments(&ids(&ids_in_order), &source)
                .expect("zero-length staged blocks are valid");

            // Assert.
            assert_eq!(plan.segments()[empty_position].length(), 0);
            assert_eq!(plan.total_length(), 12);
        }
    }

    #[test]
    fn plan_rejects_empty_list_without_lookup() {
        // Arrange.
        let source = MemMetadata::from(&[("a", 1)]);

        // Act.
        let error = plan_segments(&[], &source).expect_err("an empty commit list is invalid");

        // Assert.
        assert_eq!(error, PlanningError::EmptyList);
        assert_eq!(source.lookup_count("a"), 0);
    }

    #[test]
    fn plan_reports_missing_block_at_every_position() {
        // Arrange.
        let source = MemMetadata::from(&[("a", 4), ("b", 8)]);
        let lists = [
            ["missing", "a", "b"],
            ["a", "missing", "b"],
            ["a", "b", "missing"],
        ];

        for ids_in_order in lists {
            // Act.
            let error = plan_segments(&ids(&ids_in_order), &source)
                .expect_err("the missing block must reject the complete plan");

            // Assert.
            assert_eq!(
                error,
                PlanningError::MissingBlock(BlockId::new("missing")),
                "list {ids_in_order:?}"
            );
        }
    }

    #[test]
    fn plan_reports_first_missing_id() {
        // Arrange.
        let source = MemMetadata::from(&[("a", 4), ("b", 8)]);
        let list = ids(&["a", "first-missing", "second-missing", "b"]);

        // Act.
        let error =
            plan_segments(&list, &source).expect_err("missing metadata must reject the plan");

        // Assert.
        assert_eq!(
            error,
            PlanningError::MissingBlock(BlockId::new("first-missing"))
        );
        assert_eq!(source.lookup_count("second-missing"), 0);
        assert_eq!(source.lookup_count("b"), 0);
    }

    #[test]
    fn plan_computes_exact_u64_total() {
        // Arrange.
        let larger_than_u32 = u64::from(u32::MAX) + 1;
        let source = MemMetadata::from(&[("large", larger_than_u32), ("tail", 17)]);
        let list = ids(&["large", "tail", "large"]);

        // Act.
        let plan = plan_segments(&list, &source).expect("the exact total fits in u64");

        // Assert.
        assert_eq!(plan.total_length(), 8_589_934_609);
        assert_eq!(
            plan.segments()
                .iter()
                .map(PlannedSegment::length)
                .collect::<Vec<_>>(),
            vec![larger_than_u32, 17, larger_than_u32]
        );
    }

    #[test]
    fn plan_reports_total_length_overflow_after_prevalidation() {
        // Arrange.
        let source = MemMetadata::from(&[("max", u64::MAX), ("one", 1)]);
        let list = ids(&["max", "one"]);

        // Act.
        let error = plan_segments(&list, &source).expect_err("the ordered total exceeds u64");

        // Assert.
        assert_eq!(error, PlanningError::TotalLengthOverflow);
        assert_eq!(source.lookup_count("max"), 1);
        assert_eq!(source.lookup_count("one"), 1);
    }

    #[test]
    fn plan_handles_azure_maximum_segment_count() {
        // Arrange.
        const SEGMENTS: usize = 1_969;
        let source = MemMetadata::owned(
            (0..SEGMENTS).map(|i| (format!("block-{i:04}"), u64::try_from(i + 1).unwrap())),
        );
        let list = (0..SEGMENTS)
            .map(|i| BlockId::new(format!("block-{i:04}")))
            .collect::<Vec<_>>();

        // Act.
        let plan = plan_segments(&list, &source).expect("all maximum-count blocks are staged");

        // Assert.
        assert_eq!(plan.segments().len(), SEGMENTS);
        assert_eq!(plan.unique_sources().len(), SEGMENTS);
        assert_eq!(plan.segments()[SEGMENTS - 1].source_index(), SEGMENTS - 1);
        assert_eq!(plan.total_length(), 1_939_465);
    }

    #[test]
    fn planning_failure_bridges_into_the_storage_error_kind() {
        // Arrange.
        let error = PlanningError::MissingBlock(BlockId::new("b1"));

        // Act.
        let bridged = ovcc_core::Error::from(error);

        // Assert.
        assert_eq!(bridged.kind(), ovcc_core::ErrorKind::Storage);
        assert_eq!(bridged.message(), "committed block b1 is not staged");
    }
}
