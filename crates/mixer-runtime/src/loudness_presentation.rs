use hyper_audio_engine::{LoudnessMeasurementState, StereoLoudnessSnapshot};

/// EBU R 128 programme-loudness reference used by the relative scale.
pub const EBU_TARGET_LOUDNESS_LUFS: f32 = -23.0;

/// Selectable loudness scale ranges defined by EBU Tech 3341.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EbuLoudnessScale {
    /// -18 LU to +9 LU, or -41 LUFS to -14 LUFS.
    #[default]
    Plus9,
    /// -36 LU to +18 LU, or -59 LUFS to -5 LUFS.
    Plus18,
}

impl EbuLoudnessScale {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plus9 => "EBU +9",
            Self::Plus18 => "EBU +18",
        }
    }

    #[must_use]
    pub const fn minimum_lu(self) -> f32 {
        match self {
            Self::Plus9 => -18.0,
            Self::Plus18 => -36.0,
        }
    }

    #[must_use]
    pub const fn maximum_lu(self) -> f32 {
        match self {
            Self::Plus9 => 9.0,
            Self::Plus18 => 18.0,
        }
    }

    #[must_use]
    pub const fn minimum_lufs(self) -> f32 {
        EBU_TARGET_LOUDNESS_LUFS + self.minimum_lu()
    }

    #[must_use]
    pub const fn maximum_lufs(self) -> f32 {
        EBU_TARGET_LOUDNESS_LUFS + self.maximum_lu()
    }
}

/// Unit selected for numerical loudness-level presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoudnessDisplayUnit {
    /// Absolute K-weighted loudness in LUFS.
    #[default]
    AbsoluteLufs,
    /// Loudness relative to -23 LUFS in LU.
    RelativeLu,
}

impl LoudnessDisplayUnit {
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::AbsoluteLufs => "LUFS",
            Self::RelativeLu => "LU",
        }
    }
}

/// Complete non-visual policy for an EBU loudness scale.
///
/// The default uses the EBU +9 scale as required by EBU Tech 3341 and presents
/// absolute values. Both required scales and units remain selectable. This
/// policy does not choose UI geometry, colour, polling frequency, or certify
/// the underlying meter as EBU Mode compliant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EbuLoudnessPresentationConfig {
    scale: EbuLoudnessScale,
    unit: LoudnessDisplayUnit,
}

impl EbuLoudnessPresentationConfig {
    #[must_use]
    pub const fn new(scale: EbuLoudnessScale, unit: LoudnessDisplayUnit) -> Self {
        Self { scale, unit }
    }

    #[must_use]
    pub const fn scale(self) -> EbuLoudnessScale {
        self.scale
    }

    #[must_use]
    pub const fn unit(self) -> LoudnessDisplayUnit {
        self.unit
    }

    #[must_use]
    pub const fn minimum(self) -> f32 {
        match self.unit {
            LoudnessDisplayUnit::AbsoluteLufs => self.scale.minimum_lufs(),
            LoudnessDisplayUnit::RelativeLu => self.scale.minimum_lu(),
        }
    }

    #[must_use]
    pub const fn maximum(self) -> f32 {
        match self.unit {
            LoudnessDisplayUnit::AbsoluteLufs => self.scale.maximum_lufs(),
            LoudnessDisplayUnit::RelativeLu => self.scale.maximum_lu(),
        }
    }

    #[must_use]
    pub const fn reference(self) -> f32 {
        match self.unit {
            LoudnessDisplayUnit::AbsoluteLufs => EBU_TARGET_LOUDNESS_LUFS,
            LoudnessDisplayUnit::RelativeLu => 0.0,
        }
    }

    /// Position of the -23 LUFS reference in the selected scale.
    #[must_use]
    pub fn reference_position(self) -> f32 {
        normalized_position(EBU_TARGET_LOUDNESS_LUFS, self.scale)
    }
}

/// Whether a loudness reading lies below, within, or above the selected scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoudnessScaleRegion {
    Below,
    OnScale,
    Above,
}

/// One available loudness level mapped to a selected EBU scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaledLoudnessValue {
    absolute_lufs: f32,
    displayed: f32,
    normalized_position: f32,
    region: LoudnessScaleRegion,
}

impl ScaledLoudnessValue {
    #[must_use]
    pub const fn absolute_lufs(self) -> f32 {
        self.absolute_lufs
    }

    /// Numerical value in the selected unit, rounded to one decimal place.
    #[must_use]
    pub const fn displayed(self) -> f32 {
        self.displayed
    }

    /// Clamped position from zero at the scale minimum to one at its maximum.
    #[must_use]
    pub const fn normalized_position(self) -> f32 {
        self.normalized_position
    }

    #[must_use]
    pub const fn region(self) -> LoudnessScaleRegion {
        self.region
    }
}

/// One available maximum true-peak reading prepared for numerical display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaximumTruePeakDisplay {
    linear: f32,
    displayed_dbtp: Option<f32>,
}

impl MaximumTruePeakDisplay {
    #[must_use]
    pub const fn linear(self) -> f32 {
        self.linear
    }

    /// One-decimal dBTP value, or `None` when the measured maximum is digital
    /// zero and therefore has no finite decibel representation.
    #[must_use]
    pub const fn displayed_dbtp(self) -> Option<f32> {
        self.displayed_dbtp
    }
}

/// Target-neutral display data derived from one live loudness measurement.
///
/// Missing values remain missing rather than being replaced with a numeric
/// sentinel. Loudness Range stability remains independent from value
/// availability because 60 seconds of accepted silence can be stable without
/// producing an LRA value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessPresentationSnapshot {
    config: EbuLoudnessPresentationConfig,
    state: LoudnessMeasurementState,
    momentary: Option<ScaledLoudnessValue>,
    short_term: Option<ScaledLoudnessValue>,
    integrated: Option<ScaledLoudnessValue>,
    maximum_momentary: Option<ScaledLoudnessValue>,
    maximum_short_term: Option<ScaledLoudnessValue>,
    loudness_range_lu: Option<f32>,
    maximum_true_peak: Option<MaximumTruePeakDisplay>,
    programme_frame_count: u64,
    lra_stable: bool,
}

impl LoudnessPresentationSnapshot {
    #[must_use]
    pub fn from_measurement(
        measurement: StereoLoudnessSnapshot,
        config: EbuLoudnessPresentationConfig,
    ) -> Self {
        Self {
            config,
            state: measurement.state(),
            momentary: measurement
                .momentary_lufs()
                .map(|value| scaled_value(value, config)),
            short_term: measurement
                .short_term_lufs()
                .map(|value| scaled_value(value, config)),
            integrated: measurement
                .integrated_lufs()
                .map(|value| scaled_value(value, config)),
            maximum_momentary: measurement
                .maximum_momentary_lufs()
                .map(|value| scaled_value(value, config)),
            maximum_short_term: measurement
                .maximum_short_term_lufs()
                .map(|value| scaled_value(value, config)),
            loudness_range_lu: measurement.loudness_range_lu().map(round_one_decimal),
            maximum_true_peak: measurement.maximum_true_peak_linear().map(|linear| {
                MaximumTruePeakDisplay {
                    linear,
                    displayed_dbtp: (linear > 0.0)
                        .then(|| round_one_decimal(20.0 * linear.log10())),
                }
            }),
            programme_frame_count: measurement.programme_frame_count(),
            lra_stable: measurement.lra_stable(),
        }
    }

    #[must_use]
    pub const fn config(self) -> EbuLoudnessPresentationConfig {
        self.config
    }

    #[must_use]
    pub const fn state(self) -> LoudnessMeasurementState {
        self.state
    }

    #[must_use]
    pub const fn momentary(self) -> Option<ScaledLoudnessValue> {
        self.momentary
    }

    #[must_use]
    pub const fn short_term(self) -> Option<ScaledLoudnessValue> {
        self.short_term
    }

    #[must_use]
    pub const fn integrated(self) -> Option<ScaledLoudnessValue> {
        self.integrated
    }

    #[must_use]
    pub const fn maximum_momentary(self) -> Option<ScaledLoudnessValue> {
        self.maximum_momentary
    }

    #[must_use]
    pub const fn maximum_short_term(self) -> Option<ScaledLoudnessValue> {
        self.maximum_short_term
    }

    /// Loudness Range in LU, rounded to one decimal place.
    #[must_use]
    pub const fn loudness_range_lu(self) -> Option<f32> {
        self.loudness_range_lu
    }

    #[must_use]
    pub const fn maximum_true_peak(self) -> Option<MaximumTruePeakDisplay> {
        self.maximum_true_peak
    }

    #[must_use]
    pub const fn programme_frame_count(self) -> u64 {
        self.programme_frame_count
    }

    #[must_use]
    pub const fn lra_stable(self) -> bool {
        self.lra_stable
    }
}

fn scaled_value(absolute_lufs: f32, config: EbuLoudnessPresentationConfig) -> ScaledLoudnessValue {
    let scale = config.scale();
    let displayed = match config.unit() {
        LoudnessDisplayUnit::AbsoluteLufs => absolute_lufs,
        LoudnessDisplayUnit::RelativeLu => absolute_lufs - EBU_TARGET_LOUDNESS_LUFS,
    };
    let region = if absolute_lufs < scale.minimum_lufs() {
        LoudnessScaleRegion::Below
    } else if absolute_lufs > scale.maximum_lufs() {
        LoudnessScaleRegion::Above
    } else {
        LoudnessScaleRegion::OnScale
    };
    ScaledLoudnessValue {
        absolute_lufs,
        displayed: round_one_decimal(displayed),
        normalized_position: normalized_position(absolute_lufs, scale),
        region,
    }
}

fn normalized_position(absolute_lufs: f32, scale: EbuLoudnessScale) -> f32 {
    let minimum = scale.minimum_lufs();
    ((absolute_lufs - minimum) / (scale.maximum_lufs() - minimum)).clamp(0.0, 1.0)
}

fn round_one_decimal(value: f32) -> f32 {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

#[cfg(test)]
mod tests {
    use hyper_audio_engine::{InterleavedStereoCallback, LoudnessMeasurementCommand};

    use super::*;

    const EPSILON: f32 = 1.0e-6;

    fn assert_near(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn scale_contract_matches_ebu_plus_9_and_plus_18_ranges() {
        let plus_9 = EbuLoudnessPresentationConfig::default();
        assert_eq!(plus_9.scale(), EbuLoudnessScale::Plus9);
        assert_eq!(plus_9.unit(), LoudnessDisplayUnit::AbsoluteLufs);
        assert_eq!(plus_9.scale().name(), "EBU +9");
        assert_eq!(plus_9.unit().symbol(), "LUFS");
        assert_near(plus_9.minimum(), -41.0);
        assert_near(plus_9.maximum(), -14.0);
        assert_near(plus_9.reference(), -23.0);
        assert_near(plus_9.reference_position(), 2.0 / 3.0);

        let plus_18_relative = EbuLoudnessPresentationConfig::new(
            EbuLoudnessScale::Plus18,
            LoudnessDisplayUnit::RelativeLu,
        );
        assert_near(plus_18_relative.minimum(), -36.0);
        assert_near(plus_18_relative.maximum(), 18.0);
        assert_near(plus_18_relative.reference(), 0.0);
        assert_eq!(plus_18_relative.scale().name(), "EBU +18");
        assert_eq!(plus_18_relative.unit().symbol(), "LU");
        assert_near(plus_18_relative.reference_position(), 2.0 / 3.0);
        assert_near(EbuLoudnessScale::Plus18.minimum_lufs(), -59.0);
        assert_near(EbuLoudnessScale::Plus18.maximum_lufs(), -5.0);
    }

    #[test]
    fn values_are_rounded_classified_and_clamped_without_losing_measurement() {
        let config = EbuLoudnessPresentationConfig::new(
            EbuLoudnessScale::Plus9,
            LoudnessDisplayUnit::RelativeLu,
        );
        let below = scaled_value(-41.01, config);
        assert_eq!(below.region(), LoudnessScaleRegion::Below);
        assert_near(below.normalized_position(), 0.0);
        assert_near(below.absolute_lufs(), -41.01);
        assert_near(below.displayed(), -18.0);

        let minimum = scaled_value(-41.0, config);
        assert_eq!(minimum.region(), LoudnessScaleRegion::OnScale);
        assert_near(minimum.normalized_position(), 0.0);
        assert_near(minimum.displayed(), -18.0);

        let reference = scaled_value(-22.96, config);
        assert_eq!(reference.region(), LoudnessScaleRegion::OnScale);
        assert_near(reference.displayed(), 0.0);

        let maximum = scaled_value(-14.0, config);
        assert_eq!(maximum.region(), LoudnessScaleRegion::OnScale);
        assert_near(maximum.normalized_position(), 1.0);
        assert_near(maximum.displayed(), 9.0);

        let above = scaled_value(-13.99, config);
        assert_eq!(above.region(), LoudnessScaleRegion::Above);
        assert_near(above.normalized_position(), 1.0);
        assert_near(above.displayed(), 9.0);
    }

    #[test]
    fn presentation_preserves_unavailable_values_and_measurement_controls() {
        let mut callback = InterleavedStereoCallback::try_new(19_200, 48_000).unwrap();
        let initial = LoudnessPresentationSnapshot::from_measurement(
            callback.loudness_snapshot(),
            EbuLoudnessPresentationConfig::default(),
        );
        assert_eq!(initial.state(), LoudnessMeasurementState::Running);
        assert_eq!(initial.momentary(), None);
        assert_eq!(initial.short_term(), None);
        assert_eq!(initial.integrated(), None);
        assert_eq!(initial.maximum_momentary(), None);
        assert_eq!(initial.maximum_short_term(), None);
        assert_eq!(initial.loudness_range_lu(), None);
        assert_eq!(initial.maximum_true_peak(), None);
        assert_eq!(initial.programme_frame_count(), 0);
        assert!(!initial.lra_stable());

        let mut interleaved = vec![0.0_f32; 38_400];
        let ((), meter) = callback
            .process(&mut interleaved, |output| {
                output.left.fill(0.1);
                output.right.fill(0.1);
                Ok::<_, ()>(())
            })
            .unwrap()
            .into_parts();
        let measured = LoudnessPresentationSnapshot::from_measurement(
            meter.loudness(),
            EbuLoudnessPresentationConfig::default(),
        );
        assert!(measured.momentary().is_some());
        assert!(measured.integrated().is_some());
        assert!(measured.maximum_momentary().is_some());
        assert_eq!(measured.short_term(), None);
        assert_eq!(measured.maximum_short_term(), None);
        assert_eq!(measured.loudness_range_lu(), None);
        let true_peak = measured.maximum_true_peak().unwrap();
        assert_near(
            true_peak.displayed_dbtp().unwrap(),
            round_one_decimal(20.0 * true_peak.linear().log10()),
        );
        assert_eq!(measured.programme_frame_count(), 19_200);

        callback.apply_loudness_command(LoudnessMeasurementCommand::Reset);
        let reset = LoudnessPresentationSnapshot::from_measurement(
            callback.loudness_snapshot(),
            EbuLoudnessPresentationConfig::default(),
        );
        assert!(reset.momentary().is_some());
        assert_eq!(reset.integrated(), None);
        assert_eq!(reset.maximum_momentary(), reset.momentary());
        assert_eq!(reset.maximum_short_term(), None);
        assert_eq!(reset.maximum_true_peak(), None);
        assert_eq!(reset.programme_frame_count(), 0);
    }

    #[test]
    fn measured_digital_zero_is_distinct_from_no_true_peak_measurement() {
        let mut callback = InterleavedStereoCallback::try_new(1, 48_000).unwrap();
        let mut interleaved = [0.0_f32; 2];
        let ((), meter) = callback
            .process(&mut interleaved, |_| Ok::<_, ()>(()))
            .unwrap()
            .into_parts();
        let display = LoudnessPresentationSnapshot::from_measurement(
            meter.loudness(),
            EbuLoudnessPresentationConfig::default(),
        );
        let true_peak = display.maximum_true_peak().unwrap();
        assert_near(true_peak.linear(), 0.0);
        assert_eq!(true_peak.displayed_dbtp(), None);
    }
}
