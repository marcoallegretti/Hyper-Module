#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::PipeWireOutputStream as NativeOutputStream;
#[cfg(target_os = "windows")]
use super::WasapiDisconnectReason;
#[cfg(target_os = "windows")]
use super::WasapiOutputStream as NativeOutputStream;
use super::{
    DesktopOutputConfig, DesktopOutputStartError, DesktopOutputStopError, OutputStreamCounters,
    OutputStreamStatus,
};
use hyper_audio_engine::{
    StereoSineTestTone, StereoSineTestToneConfig, StereoSineTestToneConfigError,
};

/// One validated sine diagnostic bound to a desktop output configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopTestToneConfig {
    output: DesktopOutputConfig,
    tone: StereoSineTestToneConfig,
}

impl DesktopTestToneConfig {
    /// Creates an explicit dual-mono sine diagnostic for one desktop output.
    ///
    /// No output device is opened during validation.
    ///
    /// # Errors
    ///
    /// Returns [`StereoSineTestToneConfigError`] when frequency or level is
    /// outside the test-tone domain.
    pub fn try_new(
        output: DesktopOutputConfig,
        frequency_hz: f64,
        level_dbfs: f32,
    ) -> Result<Self, StereoSineTestToneConfigError> {
        let tone =
            StereoSineTestToneConfig::try_new(output.sample_rate_hz(), frequency_hz, level_dbfs)?;
        Ok(Self { output, tone })
    }

    #[must_use]
    pub const fn output(&self) -> &DesktopOutputConfig {
        &self.output
    }

    #[must_use]
    pub const fn tone(&self) -> StereoSineTestToneConfig {
        self.tone
    }
}

/// Exclusive native desktop output playing one continuous diagnostic sine.
#[derive(Debug)]
pub struct DesktopTestToneSession {
    output: NativeOutputStream,
    config: DesktopTestToneConfig,
}

impl DesktopTestToneSession {
    /// Starts a phase-continuous test tone on the selected desktop output.
    ///
    /// The oscillator is moved to the native backend thread and begins at a
    /// zero crossing. Output callback validation and fail-silent containment
    /// are shared with project playback.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopOutputStartError`] if the native output cannot be
    /// created or reach a usable state.
    pub fn start(config: DesktopTestToneConfig) -> Result<Self, DesktopOutputStartError> {
        let mut tone = StereoSineTestTone::new(config.tone);
        let output =
            NativeOutputStream::start(config.output.native(), move |samples: &mut [f32]| {
                tone.render_interleaved_stereo(samples)
            })
            .map_err(DesktopOutputStartError::from)?;
        Ok(Self { output, config })
    }

    #[must_use]
    pub const fn config(&self) -> &DesktopTestToneConfig {
        &self.config
    }

    #[must_use]
    pub fn counters(&self) -> OutputStreamCounters {
        self.output.counters()
    }

    #[must_use]
    pub fn output_status(&self) -> OutputStreamStatus {
        self.output.status()
    }

    /// Stops and joins the native diagnostic output within its bounded
    /// backend shutdown policy.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopOutputStopError`] if the stream failed, its thread
    /// panicked, or shutdown exceeded the backend deadline.
    pub fn stop(self) -> Result<(), DesktopOutputStopError> {
        self.output.stop().map_err(DesktopOutputStopError::from)
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.output.disconnect_reason()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OutputCallback, OutputCallbackStatus};

    #[test]
    fn desktop_config_derives_tone_rate_from_the_selected_output() {
        let output = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("diagnostic-output")
            .unwrap();
        let config = DesktopTestToneConfig::try_new(output, 1_000.0, -18.0).unwrap();

        assert_eq!(config.output().sample_rate_hz(), 48_000);
        assert_eq!(config.output().maximum_block_frames(), 256);
        assert_eq!(config.output().routing_id(), Some("diagnostic-output"));
        assert_eq!(config.tone().sample_rate_hz(), 48_000);
        assert_eq!(
            config.tone().frequency_hz().to_bits(),
            1_000.0_f64.to_bits()
        );
        assert_eq!(config.tone().level_dbfs().to_bits(), (-18.0_f32).to_bits());
    }

    #[test]
    fn desktop_callback_renders_the_shared_tone_and_reports_success() {
        let config = StereoSineTestToneConfig::try_new(48_000, 1_000.0, -18.0).unwrap();
        let mut tone = StereoSineTestTone::new(config);
        let mut callback = OutputCallback::try_new(
            move |samples: &mut [f32]| tone.render_interleaved_stereo(samples),
            8,
        )
        .unwrap();
        let counters = callback.counters();
        let mut output = [9.0_f32; 16];

        assert_eq!(
            callback.process(&mut output),
            OutputCallbackStatus::Rendered
        );
        assert!(
            output
                .chunks_exact(2)
                .all(|frame| frame[0].to_bits() == frame[1].to_bits())
        );
        assert!(output.iter().any(|sample| sample.abs() > f32::EPSILON));
        assert_eq!(counters.snapshot().rendered_callback_count, 1);
        assert_eq!(counters.snapshot().rendered_frame_count, 8);
        assert_eq!(counters.snapshot().silent_frame_count, 0);
    }
}
