use iced::widget::canvas::{self, Cache, Canvas, Frame, Geometry};
use iced::{mouse, Color, Element, Length, Rectangle, Size, Theme};

/// Number of bars to display (smoothed with decay).
const MAX_BARS: usize = 12; // 3 voices × 4 SIDs max

/// Decay factor per frame (multiplied each frame when no new data).
const DECAY: f32 = 0.92;

/// Peak hold decay (slower).
const PEAK_DECAY: f32 = 0.985;

#[derive(Debug)]
pub struct Visualizer {
    /// Current bar heights (0.0–1.0).
    bars: Vec<f32>,
    /// Peak hold values.
    peaks: Vec<f32>,
    cache: Cache,
}

impl Visualizer {
    pub fn new() -> Self {
        Self {
            bars: vec![0.0; MAX_BARS],
            peaks: vec![0.0; MAX_BARS],
            cache: Cache::new(),
        }
    }

    /// Update with new voice levels from the player.
    pub fn update(&mut self, levels: &[f32]) {
        for i in 0..MAX_BARS {
            let new_val = levels.get(i).copied().unwrap_or(0.0);

            // Rise instantly, decay gradually
            if new_val > self.bars[i] {
                self.bars[i] = new_val;
            } else {
                self.bars[i] *= DECAY;
            }

            // Peak hold
            if self.bars[i] > self.peaks[i] {
                self.peaks[i] = self.bars[i];
            } else {
                self.peaks[i] *= PEAK_DECAY;
            }
        }
        self.cache.clear();
    }

    /// Reset all bars to zero.
    pub fn reset(&mut self) {
        self.bars.fill(0.0);
        self.peaks.fill(0.0);
        self.cache.clear();
    }

    /// Number of active bars (based on last update).
    fn active_bars(&self) -> usize {
        // Show only bars that have had activity
        self.bars
            .iter()
            .rposition(|&v| v > 0.001)
            .map(|i| i + 1)
            .unwrap_or(3)
            .max(3)
    }

    pub fn view(&self) -> Element<'_, super::Message> {
        Canvas::new(self)
            .width(Length::Fill)
            .height(Length::Fixed(60.0))
            .into()
    }
}

impl canvas::Program<super::Message> for &Visualizer {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geom = self
            .cache
            .draw(renderer, bounds.size(), |frame: &mut Frame| {
                let n = self.active_bars();
                if n == 0 {
                    return;
                }

                let w = bounds.width;
                let h = bounds.height;
                let gap = 2.0;
                let bar_w = ((w - gap * (n as f32 - 1.0)) / n as f32).max(4.0);

                // Background
                frame.fill_rectangle(
                    iced::Point::ORIGIN,
                    Size::new(w, h),
                    Color::from_rgb(0.08, 0.08, 0.10),
                );

                // Color palette: cycle through C64-ish colours per SID
                let sid_colors = [
                    [
                        Color::from_rgb(0.30, 0.85, 0.55), // Green
                        Color::from_rgb(0.25, 0.75, 0.50),
                        Color::from_rgb(0.20, 0.65, 0.45),
                    ],
                    [
                        Color::from_rgb(0.40, 0.60, 0.95), // Blue
                        Color::from_rgb(0.35, 0.55, 0.85),
                        Color::from_rgb(0.30, 0.50, 0.75),
                    ],
                    [
                        Color::from_rgb(0.90, 0.55, 0.30), // Orange
                        Color::from_rgb(0.80, 0.50, 0.25),
                        Color::from_rgb(0.70, 0.45, 0.20),
                    ],
                    [
                        Color::from_rgb(0.85, 0.35, 0.55), // Pink
                        Color::from_rgb(0.75, 0.30, 0.50),
                        Color::from_rgb(0.65, 0.25, 0.45),
                    ],
                ];

                for i in 0..n {
                    let x = i as f32 * (bar_w + gap);
                    let level = self.bars[i].clamp(0.0, 1.0);
                    let bar_h = level * (h - 4.0);

                    let sid_idx = i / 3;
                    let voice_idx = i % 3;
                    let color = sid_colors
                        .get(sid_idx)
                        .and_then(|c| c.get(voice_idx))
                        .copied()
                        .unwrap_or(Color::from_rgb(0.5, 0.5, 0.5));

                    // Bar
                    if bar_h > 0.5 {
                        frame.fill_rectangle(
                            iced::Point::new(x, h - 2.0 - bar_h),
                            Size::new(bar_w, bar_h),
                            color,
                        );
                    }

                    // Peak indicator (thin line)
                    let peak = self.peaks[i].clamp(0.0, 1.0);
                    let peak_y = h - 2.0 - peak * (h - 4.0);
                    if peak > 0.01 {
                        frame.fill_rectangle(
                            iced::Point::new(x, peak_y),
                            Size::new(bar_w, 2.0),
                            Color { a: 0.8, ..color },
                        );
                    }
                }
            });

        vec![geom]
    }
}
