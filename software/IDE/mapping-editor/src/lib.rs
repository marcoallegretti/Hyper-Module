#![doc = "Transactional editing for version-1 control mappings."]
#![forbid(unsafe_code)]

mod learn;

pub use learn::{
    MappingLearnCaptureError, MappingLearnClockError, MappingLearnConfirmationError,
    MappingLearnPhase, MappingLearnRetryError, MappingLearnSession, MappingLearnStartError,
    RejectedMappingLearnConfirmation, RejectedMappingLearnStart,
};

use core::error::Error;
use core::fmt;
use std::path::Path;

use hyper_control_mappings::{
    ControlBinding, ControlMappingDocument, ControlMappingParseError, MAX_BINDINGS,
};
use hyper_mapping_compiler::{
    CompiledControlMappingTable, ControlMappingCompileError, ControlMappingResolver,
    compile_control_mapping,
};
use hyper_storage_engine::{
    ControlMappingPublicationError, ControlMappingPublicationOutcome,
    ControlMappingPublicationRequest, publish_control_mapping,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingEditorRevision(u64);

impl MappingEditorRevision {
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingEditorMutationError {
    BindingMissing,
    DuplicateBindingId,
    BindingLimit,
    CapacityUnavailable,
    RevisionExhausted,
}

impl fmt::Display for MappingEditorMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BindingMissing => "control binding does not exist",
            Self::DuplicateBindingId => "control binding identity already exists",
            Self::BindingLimit => "control binding count reached the version-1 limit",
            Self::CapacityUnavailable => "control binding editor capacity is unavailable",
            Self::RevisionExhausted => "control binding editor revision is exhausted",
        })
    }
}

impl Error for MappingEditorMutationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingConflictKind {
    SourceScopeCollision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingConflict {
    kind: MappingConflictKind,
    first_binding_index: usize,
    second_binding_index: usize,
}

impl MappingConflict {
    #[must_use]
    pub const fn kind(self) -> MappingConflictKind {
        self.kind
    }

    #[must_use]
    pub const fn first_binding_index(self) -> usize {
        self.first_binding_index
    }

    #[must_use]
    pub const fn second_binding_index(self) -> usize {
        self.second_binding_index
    }
}

#[derive(Debug)]
pub enum MappingEditorPreviewError {
    Conflict(Vec<MappingConflict>),
    CapacityUnavailable,
    Document(ControlMappingParseError),
    Compile(ControlMappingCompileError),
    Serialization(serde_json::Error),
}

impl fmt::Display for MappingEditorPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict(conflicts) => write!(
                formatter,
                "control mapping has {} blocking conflicts",
                conflicts.len()
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("control mapping preview capacity is unavailable")
            }
            Self::Document(source) => {
                write!(formatter, "edited control mapping is invalid: {source}")
            }
            Self::Compile(source) => {
                write!(
                    formatter,
                    "edited control mapping cannot be compiled: {source}"
                )
            }
            Self::Serialization(source) => {
                write!(
                    formatter,
                    "edited control mapping cannot be serialized: {source}"
                )
            }
        }
    }
}

impl Error for MappingEditorPreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Document(source) => Some(source),
            Self::Compile(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Conflict(_) | Self::CapacityUnavailable => None,
        }
    }
}

pub struct MappingEditorPreview {
    revision: MappingEditorRevision,
    expected_project_sha256: Option<[u8; 32]>,
    document: ControlMappingDocument,
    compiled: CompiledControlMappingTable,
    canonical_json: Vec<u8>,
}

impl MappingEditorPreview {
    #[must_use]
    pub const fn revision(&self) -> MappingEditorRevision {
        self.revision
    }

    /// Returns the project identity captured for durable publication, when the
    /// preview was created through [`MappingEditor::preview_for_project`].
    #[must_use]
    pub const fn expected_project_sha256(&self) -> Option<&[u8; 32]> {
        self.expected_project_sha256.as_ref()
    }

    #[must_use]
    pub const fn document(&self) -> &ControlMappingDocument {
        &self.document
    }

    #[must_use]
    pub const fn compiled(&self) -> &CompiledControlMappingTable {
        &self.compiled
    }

    #[must_use]
    pub fn canonical_json(&self) -> &[u8] {
        &self.canonical_json
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingEditorConfirmationError {
    StalePreview,
}

impl fmt::Display for MappingEditorConfirmationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("control mapping preview is stale")
    }
}

impl Error for MappingEditorConfirmationError {}

pub struct RejectedMappingConfirmation {
    reason: MappingEditorConfirmationError,
    editor: MappingEditor,
    preview: MappingEditorPreview,
}

impl RejectedMappingConfirmation {
    #[must_use]
    pub const fn reason(&self) -> MappingEditorConfirmationError {
        self.reason
    }

    #[must_use]
    pub fn into_parts(self) -> (MappingEditor, MappingEditorPreview) {
        (self.editor, self.preview)
    }
}

impl fmt::Debug for RejectedMappingConfirmation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedMappingConfirmation")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for RejectedMappingConfirmation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(formatter)
    }
}

impl Error for RejectedMappingConfirmation {}

#[derive(Debug)]
pub struct ConfirmedMappingEdit {
    expected_project_sha256: Option<[u8; 32]>,
    document: ControlMappingDocument,
    compiled: CompiledControlMappingTable,
    canonical_json: Vec<u8>,
}

impl ConfirmedMappingEdit {
    #[must_use]
    pub const fn expected_project_sha256(&self) -> Option<&[u8; 32]> {
        self.expected_project_sha256.as_ref()
    }

    #[must_use]
    pub const fn document(&self) -> &ControlMappingDocument {
        &self.document
    }

    #[must_use]
    pub const fn compiled(&self) -> &CompiledControlMappingTable {
        &self.compiled
    }

    #[must_use]
    pub fn canonical_json(&self) -> &[u8] {
        &self.canonical_json
    }

    /// Durably publishes this confirmed edit against the exact project
    /// identity used for its project-bound preview.
    ///
    /// # Errors
    ///
    /// Returns [`MappingEditorPublicationError::UnboundPreview`] when the edit
    /// was confirmed from an in-memory-only preview, or forwards a classified
    /// storage rejection. The confirmed edit remains available for retry.
    pub fn publish(
        &self,
        project_path: impl AsRef<Path>,
    ) -> Result<ControlMappingPublicationOutcome, MappingEditorPublicationError> {
        let expected_project_sha256 = self
            .expected_project_sha256
            .ok_or(MappingEditorPublicationError::UnboundPreview)?;
        publish_control_mapping(
            project_path,
            ControlMappingPublicationRequest {
                expected_project_sha256,
                mapping_bytes: &self.canonical_json,
            },
        )
        .map_err(MappingEditorPublicationError::Storage)
    }

    #[must_use]
    pub fn into_parts(self) -> (ControlMappingDocument, CompiledControlMappingTable, Vec<u8>) {
        (self.document, self.compiled, self.canonical_json)
    }
}

#[derive(Debug)]
pub enum MappingEditorPublicationError {
    UnboundPreview,
    Storage(ControlMappingPublicationError),
}

impl fmt::Display for MappingEditorPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundPreview => formatter
                .write_str("control mapping preview is not bound to a verified project identity"),
            Self::Storage(source) => source.fmt(formatter),
        }
    }
}

impl Error for MappingEditorPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(source) => Some(source),
            Self::UnboundPreview => None,
        }
    }
}

pub struct MappingEditor {
    original: ControlMappingDocument,
    bindings: Vec<ControlBinding>,
    revision: MappingEditorRevision,
}

impl MappingEditor {
    #[must_use]
    pub fn new(document: ControlMappingDocument) -> Self {
        Self {
            bindings: document.bindings().to_vec(),
            original: document,
            revision: MappingEditorRevision(0),
        }
    }

    #[must_use]
    pub const fn revision(&self) -> MappingEditorRevision {
        self.revision
    }

    #[must_use]
    pub fn bindings(&self) -> &[ControlBinding] {
        &self.bindings
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.bindings != self.original.bindings()
    }

    /// Adds one independently validated binding to the editable table.
    ///
    /// Source/scope collisions are retained for explicit conflict inspection;
    /// duplicate identities and the version-1 capacity are rejected immediately.
    ///
    /// # Errors
    ///
    /// Returns a duplicate, capacity, limit, or revision rejection without
    /// changing the table.
    pub fn insert_binding(
        &mut self,
        binding: ControlBinding,
    ) -> Result<(), MappingEditorMutationError> {
        if self.bindings.len() >= MAX_BINDINGS {
            return Err(MappingEditorMutationError::BindingLimit);
        }
        if self
            .bindings
            .iter()
            .any(|existing| existing.id() == binding.id())
        {
            return Err(MappingEditorMutationError::DuplicateBindingId);
        }
        let next_revision = self.next_revision()?;
        self.bindings
            .try_reserve(1)
            .map_err(|_| MappingEditorMutationError::CapacityUnavailable)?;
        self.bindings.push(binding);
        self.revision = next_revision;
        Ok(())
    }

    /// Replaces the binding having the replacement's stable identity.
    ///
    /// # Errors
    ///
    /// Returns a missing-binding or revision rejection without changing the
    /// table.
    pub fn replace_binding(
        &mut self,
        replacement: ControlBinding,
    ) -> Result<(), MappingEditorMutationError> {
        let Some(index) = self
            .bindings
            .iter()
            .position(|binding| binding.id() == replacement.id())
        else {
            return Err(MappingEditorMutationError::BindingMissing);
        };
        if self.bindings[index] == replacement {
            return Ok(());
        }
        let next_revision = self.next_revision()?;
        self.bindings[index] = replacement;
        self.revision = next_revision;
        Ok(())
    }

    /// Changes only the enabled state of one stable binding.
    ///
    /// # Errors
    ///
    /// Returns a missing-binding or revision rejection without changing the
    /// table.
    pub fn set_binding_enabled(
        &mut self,
        binding_id: &str,
        enabled: bool,
    ) -> Result<(), MappingEditorMutationError> {
        let Some(index) = self
            .bindings
            .iter()
            .position(|binding| binding.id() == binding_id)
        else {
            return Err(MappingEditorMutationError::BindingMissing);
        };
        if self.bindings[index].enabled() == enabled {
            return Ok(());
        }
        let next_revision = self.next_revision()?;
        self.bindings[index].set_enabled(enabled);
        self.revision = next_revision;
        Ok(())
    }

    /// Removes and returns one stable binding.
    ///
    /// # Errors
    ///
    /// Returns a missing-binding or revision rejection without changing the
    /// table.
    pub fn remove_binding(
        &mut self,
        binding_id: &str,
    ) -> Result<ControlBinding, MappingEditorMutationError> {
        let Some(index) = self
            .bindings
            .iter()
            .position(|binding| binding.id() == binding_id)
        else {
            return Err(MappingEditorMutationError::BindingMissing);
        };
        let next_revision = self.next_revision()?;
        let removed = self.bindings.remove(index);
        self.revision = next_revision;
        Ok(removed)
    }

    /// Returns every blocking version-1 source/scope collision in deterministic
    /// table-index order. A group of N colliding bindings produces N-1 linked
    /// diagnostics, covering every member with bounded output.
    ///
    /// # Errors
    ///
    /// Returns a capacity rejection without changing the editor.
    pub fn inspect_conflicts(&self) -> Result<Vec<MappingConflict>, MappingEditorMutationError> {
        let mut ordered = Vec::new();
        ordered
            .try_reserve_exact(self.bindings.len())
            .map_err(|_| MappingEditorMutationError::CapacityUnavailable)?;
        ordered.extend(self.bindings.iter().enumerate());
        ordered.sort_unstable_by(|left, right| {
            left.1
                .source()
                .cmp(right.1.source())
                .then_with(|| left.1.scope().cmp(right.1.scope()))
                .then_with(|| left.1.id().cmp(right.1.id()))
        });

        let mut conflicts = Vec::new();
        conflicts
            .try_reserve_exact(self.bindings.len().saturating_sub(1))
            .map_err(|_| MappingEditorMutationError::CapacityUnavailable)?;
        for pair in ordered.windows(2) {
            if pair[0].1.source() == pair[1].1.source() && pair[0].1.scope() == pair[1].1.scope() {
                let first_binding_index = pair[0].0.min(pair[1].0);
                let second_binding_index = pair[0].0.max(pair[1].0);
                conflicts.push(MappingConflict {
                    kind: MappingConflictKind::SourceScopeCollision,
                    first_binding_index,
                    second_binding_index,
                });
            }
        }
        conflicts.sort_unstable_by_key(|conflict| {
            (conflict.first_binding_index, conflict.second_binding_index)
        });
        Ok(conflicts)
    }

    /// Validates, compiles, and serializes the current edit without modifying
    /// the original mapping or any project files.
    ///
    /// # Errors
    ///
    /// Returns blocking conflicts, allocation failure, document rejection,
    /// project-resolution failure, or canonical serialization failure.
    pub fn preview(
        &self,
        resolver: &impl ControlMappingResolver,
    ) -> Result<MappingEditorPreview, MappingEditorPreviewError> {
        self.build_preview(resolver, None)
    }

    /// Creates a compiled preview bound to the exact verified project manifest
    /// identity that supplied the resolver. Only this preview form can later be
    /// published by [`ConfirmedMappingEdit::publish`].
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::preview`].
    pub fn preview_for_project(
        &self,
        resolver: &impl ControlMappingResolver,
        expected_project_sha256: [u8; 32],
    ) -> Result<MappingEditorPreview, MappingEditorPreviewError> {
        self.build_preview(resolver, Some(expected_project_sha256))
    }

    fn build_preview(
        &self,
        resolver: &impl ControlMappingResolver,
        expected_project_sha256: Option<[u8; 32]>,
    ) -> Result<MappingEditorPreview, MappingEditorPreviewError> {
        let conflicts = self
            .inspect_conflicts()
            .map_err(|_| MappingEditorPreviewError::CapacityUnavailable)?;
        if !conflicts.is_empty() {
            return Err(MappingEditorPreviewError::Conflict(conflicts));
        }

        let mut bindings = self.bindings.clone();
        bindings.sort_unstable_by(|left, right| left.id().cmp(right.id()));
        let document = ControlMappingDocument::try_new(
            self.original.id().to_owned(),
            self.original.project_id().to_owned(),
            bindings,
            self.original.extensions().clone(),
        )
        .map_err(MappingEditorPreviewError::Document)?;
        let compiled = compile_control_mapping(&document, document.id(), resolver)
            .map_err(MappingEditorPreviewError::Compile)?;
        let canonical_json = document
            .to_canonical_json()
            .map_err(MappingEditorPreviewError::Serialization)?;
        Ok(MappingEditorPreview {
            revision: self.revision,
            expected_project_sha256,
            document,
            compiled,
            canonical_json,
        })
    }

    /// Confirms an unchanged preview and consumes the edit transaction.
    ///
    /// This produces a validated publication payload but performs no filesystem
    /// write. Persistence remains a separate project transaction.
    ///
    /// # Errors
    ///
    /// Returns both the editor and preview when any edit occurred after the
    /// preview or the preview describes different content.
    pub fn confirm(
        self,
        preview: MappingEditorPreview,
    ) -> Result<ConfirmedMappingEdit, Box<RejectedMappingConfirmation>> {
        if self.revision != preview.revision || !self.matches_preview(&preview.document) {
            return Err(Box::new(RejectedMappingConfirmation {
                reason: MappingEditorConfirmationError::StalePreview,
                editor: self,
                preview,
            }));
        }
        Ok(ConfirmedMappingEdit {
            expected_project_sha256: preview.expected_project_sha256,
            document: preview.document,
            compiled: preview.compiled,
            canonical_json: preview.canonical_json,
        })
    }

    /// Cancels every edit and returns the original validated document.
    #[must_use]
    pub fn cancel(self) -> ControlMappingDocument {
        self.original
    }

    fn next_revision(&self) -> Result<MappingEditorRevision, MappingEditorMutationError> {
        self.revision
            .0
            .checked_add(1)
            .map(MappingEditorRevision)
            .ok_or(MappingEditorMutationError::RevisionExhausted)
    }

    fn matches_preview(&self, document: &ControlMappingDocument) -> bool {
        self.original.id() == document.id()
            && self.original.project_id() == document.project_id()
            && self.original.extensions() == document.extensions()
            && self.bindings.len() == document.bindings().len()
            && self.bindings.iter().all(|binding| {
                document
                    .bindings()
                    .binary_search_by(|candidate| candidate.id().cmp(binding.id()))
                    .is_ok_and(|index| document.bindings()[index] == *binding)
            })
    }
}
