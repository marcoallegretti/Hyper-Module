use core::error::Error;
use core::fmt;
use std::time::Instant;

use hyper_control_mappings::{ControlBinding, ControlMappingParseError, ControlSource};

use super::{MappingEditor, MappingEditorMutationError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingLearnStartError {
    BindingMissing,
    DeadlineNotFuture,
}

impl fmt::Display for MappingLearnStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BindingMissing => "control binding does not exist",
            Self::DeadlineNotFuture => "mapping Learn deadline must be in the future",
        })
    }
}

impl Error for MappingLearnStartError {}

pub struct RejectedMappingLearnStart {
    reason: MappingLearnStartError,
    editor: MappingEditor,
}

impl RejectedMappingLearnStart {
    #[must_use]
    pub const fn reason(&self) -> MappingLearnStartError {
        self.reason
    }

    #[must_use]
    pub fn into_editor(self) -> MappingEditor {
        self.editor
    }
}

impl fmt::Debug for RejectedMappingLearnStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedMappingLearnStart")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for RejectedMappingLearnStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(formatter)
    }
}

impl Error for RejectedMappingLearnStart {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingLearnClockError {
    ClockRegressed,
}

impl fmt::Display for MappingLearnClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mapping Learn monotonic clock regressed")
    }
}

impl Error for MappingLearnClockError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingLearnPhase<'a> {
    Armed {
        deadline: Instant,
    },
    Candidate {
        source: &'a ControlSource,
        captured_at: Instant,
    },
    TimedOut {
        deadline: Instant,
    },
}

#[derive(Debug)]
pub enum MappingLearnCaptureError {
    ClockRegressed,
    TimedOut,
    CandidateAlreadyCaptured,
    IncompatibleSource(ControlMappingParseError),
}

impl fmt::Display for MappingLearnCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockRegressed => formatter.write_str("mapping Learn monotonic clock regressed"),
            Self::TimedOut => formatter.write_str("mapping Learn timed out before source capture"),
            Self::CandidateAlreadyCaptured => {
                formatter.write_str("mapping Learn already has a temporary source candidate")
            }
            Self::IncompatibleSource(source) => {
                write!(
                    formatter,
                    "learned source is incompatible with the binding: {source}"
                )
            }
        }
    }
}

impl Error for MappingLearnCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::IncompatibleSource(source) => Some(source),
            Self::ClockRegressed | Self::TimedOut | Self::CandidateAlreadyCaptured => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingLearnRetryError {
    ClockRegressed,
    DeadlineNotFuture,
}

impl fmt::Display for MappingLearnRetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ClockRegressed => "mapping Learn monotonic clock regressed",
            Self::DeadlineNotFuture => "mapping Learn retry deadline must be in the future",
        })
    }
}

impl Error for MappingLearnRetryError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingLearnConfirmationError {
    ClockRegressed,
    TimedOut,
    CandidateMissing,
    Mutation(MappingEditorMutationError),
}

impl fmt::Display for MappingLearnConfirmationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockRegressed => formatter.write_str("mapping Learn monotonic clock regressed"),
            Self::TimedOut => formatter.write_str("mapping Learn timed out before source capture"),
            Self::CandidateMissing => {
                formatter.write_str("mapping Learn has no source candidate to confirm")
            }
            Self::Mutation(source) => source.fmt(formatter),
        }
    }
}

impl Error for MappingLearnConfirmationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mutation(source) => Some(source),
            Self::ClockRegressed | Self::TimedOut | Self::CandidateMissing => None,
        }
    }
}

pub struct RejectedMappingLearnConfirmation {
    reason: MappingLearnConfirmationError,
    session: MappingLearnSession,
}

impl RejectedMappingLearnConfirmation {
    #[must_use]
    pub const fn reason(&self) -> MappingLearnConfirmationError {
        self.reason
    }

    #[must_use]
    pub fn into_session(self) -> MappingLearnSession {
        self.session
    }
}

impl fmt::Debug for RejectedMappingLearnConfirmation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RejectedMappingLearnConfirmation")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for RejectedMappingLearnConfirmation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(formatter)
    }
}

impl Error for RejectedMappingLearnConfirmation {}

pub struct MappingLearnSession {
    editor: MappingEditor,
    binding_index: usize,
    deadline: Instant,
    last_observed_at: Instant,
    candidate: Option<(ControlBinding, Instant)>,
}

impl MappingLearnSession {
    fn new(editor: MappingEditor, binding_index: usize, now: Instant, deadline: Instant) -> Self {
        Self {
            editor,
            binding_index,
            deadline,
            last_observed_at: now,
            candidate: None,
        }
    }

    #[must_use]
    pub fn binding_id(&self) -> &str {
        self.editor.bindings()[self.binding_index].id()
    }

    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    #[must_use]
    pub const fn editor(&self) -> &MappingEditor {
        &self.editor
    }

    /// Returns the time-derived Learn phase and advances the session's
    /// monotonic observation boundary.
    ///
    /// # Errors
    ///
    /// Rejects a timestamp older than the last timestamp accepted by this
    /// session. A captured candidate remains available after the armed deadline.
    pub fn phase(&mut self, now: Instant) -> Result<MappingLearnPhase<'_>, MappingLearnClockError> {
        if now < self.last_observed_at {
            return Err(MappingLearnClockError::ClockRegressed);
        }
        self.last_observed_at = now;
        if let Some((candidate, captured_at)) = &self.candidate {
            return Ok(MappingLearnPhase::Candidate {
                source: candidate.source(),
                captured_at: *captured_at,
            });
        }
        if now >= self.deadline {
            Ok(MappingLearnPhase::TimedOut {
                deadline: self.deadline,
            })
        } else {
            Ok(MappingLearnPhase::Armed {
                deadline: self.deadline,
            })
        }
    }

    /// Captures the first schema-compatible typed source as a temporary
    /// candidate. The editor is not mutated until [`Self::confirm`].
    ///
    /// # Errors
    ///
    /// Rejects regressed time, observations at or after the deadline, a second
    /// candidate before retry, or a source incompatible with the binding's
    /// existing transform and target.
    pub fn observe(
        &mut self,
        now: Instant,
        source: ControlSource,
    ) -> Result<(), MappingLearnCaptureError> {
        if now < self.last_observed_at {
            return Err(MappingLearnCaptureError::ClockRegressed);
        }
        if self.candidate.is_some() {
            return Err(MappingLearnCaptureError::CandidateAlreadyCaptured);
        }
        self.last_observed_at = now;
        if now >= self.deadline {
            return Err(MappingLearnCaptureError::TimedOut);
        }

        let current = &self.editor.bindings()[self.binding_index];
        let candidate = ControlBinding::try_new(
            current.id().to_owned(),
            current.enabled(),
            source,
            current.scope().clone(),
            current.transform(),
            current.target().clone(),
        )
        .map_err(MappingLearnCaptureError::IncompatibleSource)?;
        self.candidate = Some((candidate, now));
        Ok(())
    }

    /// Discards the temporary candidate and arms the same binding again.
    ///
    /// # Errors
    ///
    /// Rejects regressed time or a deadline that is not strictly in the future.
    pub fn retry(&mut self, now: Instant, deadline: Instant) -> Result<(), MappingLearnRetryError> {
        if now < self.last_observed_at {
            return Err(MappingLearnRetryError::ClockRegressed);
        }
        if deadline <= now {
            return Err(MappingLearnRetryError::DeadlineNotFuture);
        }
        self.deadline = deadline;
        self.last_observed_at = now;
        self.candidate = None;
        Ok(())
    }

    /// Applies the temporary source candidate as one normal editor mutation.
    /// Candidates captured before the deadline remain confirmable afterward.
    ///
    /// # Errors
    ///
    /// Returns the complete Learn session for retry or cancellation when time
    /// regressed, capture timed out, no candidate exists, or editor mutation
    /// cannot advance its revision.
    pub fn confirm(
        mut self,
        now: Instant,
    ) -> Result<MappingEditor, Box<RejectedMappingLearnConfirmation>> {
        let reason = if now < self.last_observed_at {
            Some(MappingLearnConfirmationError::ClockRegressed)
        } else if self.candidate.is_none() && now >= self.deadline {
            Some(MappingLearnConfirmationError::TimedOut)
        } else if self.candidate.is_none() {
            Some(MappingLearnConfirmationError::CandidateMissing)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(Box::new(RejectedMappingLearnConfirmation {
                reason,
                session: self,
            }));
        }

        let Some((candidate, _)) = self.candidate.as_ref() else {
            return Err(Box::new(RejectedMappingLearnConfirmation {
                reason: MappingLearnConfirmationError::CandidateMissing,
                session: self,
            }));
        };
        let candidate = candidate.clone();
        if let Err(source) = self.editor.replace_binding(candidate) {
            return Err(Box::new(RejectedMappingLearnConfirmation {
                reason: MappingLearnConfirmationError::Mutation(source),
                session: self,
            }));
        }
        Ok(self.editor)
    }

    /// Cancels Learn without changing the editor.
    #[must_use]
    pub fn cancel(self) -> MappingEditor {
        self.editor
    }
}

impl fmt::Debug for MappingLearnSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MappingLearnSession")
            .field("binding_id", &self.binding_id())
            .field("deadline", &self.deadline)
            .field("last_observed_at", &self.last_observed_at)
            .field(
                "candidate",
                &self
                    .candidate
                    .as_ref()
                    .map(|(binding, captured_at)| (binding.source(), captured_at)),
            )
            .finish_non_exhaustive()
    }
}

impl MappingEditor {
    /// Moves this editor into a bounded Learn session for one existing binding.
    ///
    /// # Errors
    ///
    /// Returns the editor unchanged when the binding is absent or the supplied
    /// monotonic deadline is not strictly later than `now`.
    pub fn begin_learn(
        self,
        binding_id: &str,
        now: Instant,
        deadline: Instant,
    ) -> Result<MappingLearnSession, Box<RejectedMappingLearnStart>> {
        if deadline <= now {
            return Err(Box::new(RejectedMappingLearnStart {
                reason: MappingLearnStartError::DeadlineNotFuture,
                editor: self,
            }));
        }
        let Some(binding_index) = self
            .bindings()
            .iter()
            .position(|binding| binding.id() == binding_id)
        else {
            return Err(Box::new(RejectedMappingLearnStart {
                reason: MappingLearnStartError::BindingMissing,
                editor: self,
            }));
        };
        Ok(MappingLearnSession::new(self, binding_index, now, deadline))
    }
}
