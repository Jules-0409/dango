//! Detail card: a second non-activating glass panel beside the capsule.
//!
//! Built with `CATextLayer` (system font → PingFang on macOS) and
//! `CAShapeLayer` bars, laid out manually. It fades in/out natively and flips to
//! the left when the capsule sits near the right screen edge.

use dango_lib::models::{Bucket, PlanQuota};

use super::card_layer::CardLayer;
use crate::app_model::{CARD_GAP, SCREEN_MARGIN};

pub const CARD_WIDTH: f64 = 260.0;
/// Draw-root clip + background shape share this radius — the card reads as a
/// rounded glass tile, not a sharp slab.
pub const CARD_RADIUS: f64 = 18.0;

const PADDING_X: f64 = 16.0;
const PADDING_TOP: f64 = 16.0;
const PADDING_BOTTOM: f64 = 16.0;
const TITLE_SIZE: f64 = 14.0;
/// Line height to reserve for the title. PingFang SC (chosen over the
/// requested Optima for CJK) rises higher than Optima, so reserving exactly
/// `TITLE_SIZE` overlaps the subtitle; matching AppKit's own ascent fixes it.
const TITLE_LINE_HEIGHT: f64 = 19.0;
const SUB_SIZE: f64 = 10.0;
/// 大数字右侧预留宽（整数 22pt + 小 % 11pt + 间隙），名称/副标题都给它让位。
const HEADLINE_WIDTH: f64 = 58.0;
const PERCENT_BIG_SIZE: f64 = 22.0;
const PERCENT_SMALL_SIZE: f64 = 11.0;
const PCT_GLYPH_WIDTH: f64 = 9.0;
const GROUP_GAP: f64 = 8.0;
const GROUP_SIZE: f64 = 10.0;
const GROUP_LINE_HEIGHT: f64 = 13.0;
const ROW_SIZE: f64 = 11.0;
const ROW_HEIGHT: f64 = 22.0;
const BAR_HEIGHT: f64 = 5.0;
const ROW_NAME_WIDTH: f64 = 72.0;
const ROW_VALUE_WIDTH: f64 = 30.0;
/// Rows with no percentage show their detail text across the bar's space.
const ROW_DETAIL_WIDTH: f64 = 140.0;
const PROXY_SIZE: f64 = 10.0;
const PROXY_ROW_HEIGHT: f64 = 24.0;
const ERROR_BLOCK_PAD: f64 = 10.0;
const ERROR_HINT_SIZE: f64 = 11.0;
const ERROR_LINE_HEIGHT: f64 = 14.0;
const ERROR_DETAIL_SIZE: f64 = 10.0;
const ERROR_DETAIL_LINE_HEIGHT: f64 = 13.0;
const FOOTER_SIZE: f64 = 9.5;

/// Where the card should appear relative to the capsule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// What a line does after it is rendered. `Plain` lines never change;
/// `Countdown` and `Freshness` are recomputed once a second while the card
/// is visible.
#[derive(Clone, Debug)]
pub enum LineRole {
    Plain,
    /// Live countdown to this Unix-ms reset timestamp.
    Countdown(u64),
    /// "Ns 前更新" age of the snapshot (Unix seconds) — greys out / warns
    /// when the data is stale.
    Freshness(u64),
}

/// One line of card text.
#[derive(Clone, Debug)]
pub struct TextSpec {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: Option<f64>,
    pub size: f64,
    pub weight: u32,
    pub color: Rgba,
    pub font: super::card_font::FontKind,
    pub align: TextAlign,
    pub role: LineRole,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba(pub f64, pub f64, pub f64, pub f64);

// 主版芭乐玻璃色板（与 `ui/common.css` 的 `--ok/--warn/--danger` 对齐）：
// 饱和度预算给球，状态色走温润低饱和，不用高亮 Material Web 色。
/// Everything the card body paints, swapped as a unit for light/dark glass.
#[derive(Clone, Copy)]
pub struct CardPalette {
    pub bg: Rgba,
    pub hairline: Rgba,
    pub track: Rgba,
    pub separator: Rgba,
    pub ink: Rgba,
    pub ink2: Rgba,
    pub ink3: Rgba,
    pub ink4: Rgba,
    pub ok: Rgba,
    pub warn: Rgba,
    pub danger: Rgba,
    pub rose: Rgba,
}

const DARK: CardPalette = CardPalette {
    bg: Rgba(0.10, 0.10, 0.11, 0.96),
    hairline: Rgba(1.0, 1.0, 1.0, 0.10),
    track: Rgba(1.0, 1.0, 1.0, 0.06),
    separator: Rgba(1.0, 1.0, 1.0, 0.06),
    ink: Rgba(0.925, 0.894, 0.878, 1.0),
    ink2: Rgba(0.784, 0.749, 0.729, 1.0),
    ink3: Rgba(0.604, 0.561, 0.545, 1.0),
    // 3:1 对比度以下看不清；这个色相把毛玻璃深底上的副文本拉到约 4.6:1。
    ink4: Rgba(0.639, 0.604, 0.588, 1.0),
    ok: Rgba(0.310, 0.616, 0.431, 1.0),     // #4F9D6E
    warn: Rgba(0.753, 0.475, 0.102, 1.0),   // #C0791A
    danger: Rgba(0.780, 0.290, 0.247, 1.0), // #C74A3F
    rose: Rgba(0.780, 0.675, 0.675, 1.0),
};

const LIGHT: CardPalette = CardPalette {
    // 白玻璃底：接近实色，小字不被背后内容干扰；发丝线走深色。
    bg: Rgba(0.98, 0.97, 0.96, 0.97),
    hairline: Rgba(0.0, 0.0, 0.0, 0.10),
    track: Rgba(0.0, 0.0, 0.0, 0.08),
    separator: Rgba(0.0, 0.0, 0.0, 0.09),
    ink: Rgba(0.13, 0.12, 0.11, 1.0),
    ink2: Rgba(0.30, 0.28, 0.26, 1.0),
    ink3: Rgba(0.45, 0.42, 0.40, 1.0),
    ink4: Rgba(0.52, 0.49, 0.46, 1.0),
    // 状态色压深一档，在浅底上保持芭乐玻璃的低饱和气质。
    ok: Rgba(0.20, 0.50, 0.32, 1.0),
    warn: Rgba(0.63, 0.39, 0.07, 1.0),
    danger: Rgba(0.68, 0.24, 0.20, 1.0),
    rose: Rgba(0.68, 0.42, 0.42, 1.0),
};

pub fn card_palette(dark: bool) -> CardPalette {
    if dark {
        DARK
    } else {
        LIGHT
    }
}

/// The live mini ball sitting in the card header — the same face as the
/// capsule slot, so the card reads as "the ball opened up" rather than a
/// detached panel. Rebuilt only when (shape, color, emotion) changes.
struct MiniBall {
    ball: grok_ball::Ball,
    view: super::ball_view::BallView,
    key: (String, String, String),
}

pub struct CardView {
    layers: CardLayer,
    plan_id: Option<String>,
    height: f64,
    proxy_row: Option<(f64, f64)>,
    mini: Option<MiniBall>,
    pinned: bool,
    dark: bool,
}

const MINI_SLOT: f64 = 30.0;
const MINI_SIDE: f64 = 25.0;

impl CardView {
    pub fn new(parent: &objc2_quartz_core::CALayer, scale: f64, dark: bool) -> Self {
        let mut layers = CardLayer::new(parent, scale);
        layers.set_dark(dark);
        Self {
            layers,
            plan_id: None,
            height: 0.0,
            proxy_row: None,
            mini: None,
            pinned: false,
            dark,
        }
    }

    /// Swap the body palette between dark and light glass; the next
    /// `show_plan` repaints every label with the new ink set.
    pub fn set_dark(&mut self, dark: bool) {
        self.dark = dark;
        self.layers.set_dark(dark);
        for mini in self.mini.iter_mut() {
            mini.view.set_track_color(dark);
        }
    }

    pub fn is_visible(&self) -> bool {
        self.layers.is_visible()
    }

    pub fn height(&self) -> f64 {
        self.height
    }

    pub fn plan_id(&self) -> Option<&str> {
        self.plan_id.as_deref()
    }

    /// Advance the header mini ball one animation frame.
    pub fn tick_mini(&mut self, now_ms: f64) {
        if let Some(mini) = self.mini.as_mut() {
            mini.ball.tick_fast(now_ms);
            mini.view.draw(mini.ball.frame());
        }
    }

    /// Pop-in animation: fade + slide from the capsule side + a hair of scale.
    /// Fire-and-forget CA animations — no completion handlers, so nothing can
    /// be left half-faded the way the old transaction fade was.
    pub fn animate_bars_in(&self) {
        self.layers.animate_bars_in();
    }

    pub fn animate_in(&self, side: CardSide) {
        self.layers.animate_in(match side {
            CardSide::Right => -9.0,
            CardSide::Left => 9.0,
        });
        self.layers.animate_bars_in();
    }

    pub fn proxy_row_contains(&self, x: f64, y: f64) -> bool {
        (PADDING_X..=CARD_WIDTH - PADDING_X).contains(&x)
            && self
                .proxy_row
                .is_some_and(|(top, bottom)| (top..=bottom).contains(&y))
    }

    /// Rebuild the card contents for `plan`. Returns the computed height.
    ///
    /// `fetched_at` is the snapshot's Unix-seconds timestamp for the footer
    /// freshness line; `pinned` marks the card so the footer can say so.
    pub fn show_plan(
        &mut self,
        plan: &PlanQuota,
        settings: &dango_lib::Settings,
        fetched_at: u64,
        now_ms: u64,
        pinned: bool,
    ) -> f64 {
        // Remember whether the card is switching plans — the pop-in and the
        // mini-ball draw-in only fire on a real change, not on every re-layout
        // (a capsule drag calls show_plan each Moved event).
        let plan_changed = self.plan_id.as_deref() != Some(plan.id.as_str());
        self.plan_id = Some(plan.id.clone());
        self.pinned = pinned;
        self.proxy_row = None;
        self.layers.reset();
        let pal = card_palette(self.dark);

        let plan_rgb = crate::theme::color_for_plan(&plan.id, settings);
        let plan_rgb = crate::theme::hex_to_rgb(&plan_rgb)
            .map(|(r, g, b)| Rgba(r, g, b, 1.0))
            .unwrap_or(pal.ok);

        let mut y = PADDING_TOP;
        let mut lines: Vec<TextSpec> = Vec::new();

        // Header: live mini ball (with its own ring) + name + subtitle on the
        // left, big percentage on the right.
        let shape = crate::theme::shape_for(&plan.id, settings);
        let color = crate::theme::color_for_plan(&plan.id, settings);
        let emotion = crate::theme::emotion_for(plan).to_string();
        let key = (
            format!("{shape:?}").to_lowercase(),
            color.clone(),
            emotion.clone(),
        );
        if self.mini.as_ref().map(|m| &m.key) != Some(&key) {
            let view = super::ball_view::BallView::new(
                self.layers.root_layer(),
                (PADDING_X, y - 1.0),
                MINI_SLOT,
                MINI_SIDE,
                self.layers.scale(),
            );
            let mut ball = grok_ball::Ball::new(grok_ball::BallOptions {
                shape,
                color: Some(color),
                eye_color: Some(crate::theme::eye_color().to_string()),
                emotion: Some(emotion),
                lite: true,
                eye_scale: 1.15,
                ..Default::default()
            });
            ball.render_static(now_ms as f64);
            self.mini = Some(MiniBall { ball, view, key });
        }
        if let Some(mini) = self.mini.as_mut() {
            mini.view.set_position((PADDING_X, y - 1.0));
            mini.view
                .set_ring(&crate::theme::ring_style(plan, settings));
            mini.view.draw(mini.ball.frame());
            // The card's own hello: its ring redraws a beat after the pop.
            if plan_changed {
                mini.view.animate_ring_enter(0.08);
            }
        }

        let text_left = PADDING_X + 38.0;
        let text_width = CARD_WIDTH - PADDING_X - HEADLINE_WIDTH - 6.0 - text_left;
        lines.push(TextSpec {
            text: plan.name.clone(),
            x: text_left,
            y: y + 2.0,
            width: Some(text_width),
            size: TITLE_SIZE,
            weight: 600,
            color: pal.ink,
            font: super::card_font::FontKind::Display,
            align: TextAlign::Left,
            role: LineRole::Plain,
        });

        // Big number in ink (only a low quota takes its warn/danger colour),
        // with a small "%" tucked after it at the right edge. None on error.
        if plan.ok {
            if let Some(percent) = plan.remaining_percent {
                let headline_right = CARD_WIDTH - PADDING_X;
                let headline_color = if percent < 20.0 {
                    percent_color(plan, &pal)
                } else {
                    pal.ink
                };
                lines.push(TextSpec {
                    text: format!("{}", percent.round() as i64),
                    x: headline_right - PCT_GLYPH_WIDTH - HEADLINE_WIDTH,
                    y: y - 1.0,
                    width: Some(HEADLINE_WIDTH),
                    size: PERCENT_BIG_SIZE,
                    weight: 600,
                    color: headline_color,
                    font: super::card_font::FontKind::Mono,
                    align: TextAlign::Right,
                    role: LineRole::Plain,
                });
                lines.push(TextSpec {
                    text: "%".into(),
                    x: headline_right - PCT_GLYPH_WIDTH,
                    y: y + 8.0,
                    width: Some(PCT_GLYPH_WIDTH),
                    size: PERCENT_SMALL_SIZE,
                    weight: 500,
                    color: pal.ink3,
                    font: super::card_font::FontKind::Mono,
                    align: TextAlign::Right,
                    role: LineRole::Plain,
                });
            }
        }

        // Subtitle: the headline label with a live reset countdown when the
        // provider supplies one; the plan's note when not; the actionable
        // hint on failure.
        let (subtitle, role) = if plan.ok {
            match plan
                .headline_label
                .as_deref()
                .map(str::trim)
                .filter(|label| !label.is_empty())
            {
                Some(label) => {
                    let text = match plan.resets_at {
                        Some(at) => format!("{label} · {}", format_reset(at, now_ms)),
                        None => label.to_string(),
                    };
                    (
                        text,
                        plan.resets_at
                            .map(LineRole::Countdown)
                            .unwrap_or(LineRole::Plain),
                    )
                }
                None => (plan.note.clone().unwrap_or_default(), LineRole::Plain),
            }
        } else {
            (
                crate::theme::error_hint(plan.error.as_deref()).to_string(),
                LineRole::Plain,
            )
        };
        if !subtitle.is_empty() {
            lines.push(TextSpec {
                text: subtitle,
                x: text_left,
                y: y + TITLE_LINE_HEIGHT + 1.0,
                width: Some(CARD_WIDTH - PADDING_X * 2.0 - 38.0),
                size: SUB_SIZE,
                weight: 400,
                color: if plan.ok { pal.ink3 } else { pal.warn },
                font: super::card_font::FontKind::Ui,
                align: TextAlign::Left,
                role,
            });
        }
        y += MINI_SLOT + 8.0;

        if plan.ok {
            let row_value_right = CARD_WIDTH - PADDING_X;
            let bar_width =
                row_value_right - (ROW_VALUE_WIDTH + 8.0) - (PADDING_X + ROW_NAME_WIDTH + 8.0);
            let mut first_group = true;
            for group in group_buckets(&plan.buckets) {
                if !first_group {
                    y += GROUP_GAP;
                }
                first_group = false;

                // Only named groups get a heading; singletons without a
                // " · " in the label flow under the header.
                if let Some(name) = group.name {
                    lines.push(TextSpec {
                        text: name.into(),
                        x: PADDING_X,
                        y,
                        width: Some(CARD_WIDTH - PADDING_X * 2.0),
                        size: GROUP_SIZE,
                        weight: 600,
                        color: pal.ink4,
                        font: super::card_font::FontKind::Ui,
                        align: TextAlign::Left,
                        role: LineRole::Plain,
                    });
                    y += GROUP_LINE_HEIGHT;
                }

                for (_full_label, row_name, bucket) in &group.rows {
                    let row_mid_y = row_mid(y);
                    match bucket.remaining_percent {
                        Some(percent) => {
                            let pct_color = metric_color(plan.ok, Some(percent), plan_rgb, &pal);
                            lines.push(TextSpec {
                                text: (*row_name).into(),
                                x: PADDING_X,
                                y: text_top(row_mid_y, ROW_SIZE),
                                width: Some(ROW_NAME_WIDTH),
                                size: ROW_SIZE,
                                weight: 400,
                                color: pal.ink3,
                                font: super::card_font::FontKind::Ui,
                                align: TextAlign::Left,
                                role: LineRole::Plain,
                            });
                            self.layers.bar(
                                PADDING_X + ROW_NAME_WIDTH + 8.0,
                                row_mid_y - BAR_HEIGHT / 2.0,
                                bar_width,
                                BAR_HEIGHT,
                                (percent / 100.0).clamp(0.0, 1.0),
                                pct_color,
                            );
                            // Under 20% remaining the number takes the
                            // metric color; healthy rows stay neutral ink.
                            let value_color = if percent < 20.0 { pct_color } else { pal.ink };
                            lines.push(TextSpec {
                                text: format!("{}", percent.round() as i64),
                                x: row_value_right - ROW_VALUE_WIDTH,
                                y: text_top(row_mid_y, ROW_SIZE),
                                width: Some(ROW_VALUE_WIDTH),
                                size: ROW_SIZE,
                                weight: 600,
                                color: value_color,
                                font: super::card_font::FontKind::Mono,
                                align: TextAlign::Right,
                                role: LineRole::Plain,
                            });
                        }
                        None => {
                            // No percentage (Credits & co): no bar, the detail
                            // text right-aligned across the bar's space.
                            lines.push(TextSpec {
                                text: (*row_name).into(),
                                x: PADDING_X,
                                y: text_top(row_mid_y, ROW_SIZE),
                                width: Some(ROW_NAME_WIDTH),
                                size: ROW_SIZE,
                                weight: 400,
                                color: pal.ink3,
                                font: super::card_font::FontKind::Ui,
                                align: TextAlign::Left,
                                role: LineRole::Plain,
                            });
                            let detail_text = bucket
                                .detail
                                .as_deref()
                                .map(str::trim)
                                .filter(|detail| !detail.is_empty())
                                .unwrap_or("—");
                            lines.push(TextSpec {
                                text: detail_text.into(),
                                x: row_value_right - ROW_DETAIL_WIDTH,
                                y: text_top(row_mid_y, ROW_SIZE),
                                width: Some(ROW_DETAIL_WIDTH),
                                size: ROW_SIZE - 1.0,
                                weight: 400,
                                color: pal.ink4,
                                font: super::card_font::FontKind::Mono,
                                align: TextAlign::Right,
                                role: LineRole::Plain,
                            });
                        }
                    }
                    y += ROW_HEIGHT;
                }
            }
        } else {
            // Error state: a tinted block with the human hint on top and the
            // raw technical error below (small mono, subdued).
            let block_height =
                ERROR_BLOCK_PAD * 2.0 + ERROR_LINE_HEIGHT + 4.0 + ERROR_DETAIL_LINE_HEIGHT;
            self.layers.box_container(
                PADDING_X,
                y,
                CARD_WIDTH - PADDING_X * 2.0,
                block_height,
                12.0,
                Rgba(pal.danger.0, pal.danger.1, pal.danger.2, 0.11),
                None,
            );
            lines.push(TextSpec {
                text: crate::theme::error_hint(plan.error.as_deref()).to_string(),
                x: PADDING_X + ERROR_BLOCK_PAD,
                y: y + ERROR_BLOCK_PAD,
                width: Some(CARD_WIDTH - PADDING_X * 2.0 - ERROR_BLOCK_PAD * 2.0),
                size: ERROR_HINT_SIZE,
                weight: 500,
                color: pal.ink,
                font: super::card_font::FontKind::Ui,
                align: TextAlign::Left,
                role: LineRole::Plain,
            });
            let err_msg = plan.error.clone().unwrap_or_else(|| "查询失败".into());
            lines.push(TextSpec {
                text: err_msg,
                x: PADDING_X + ERROR_BLOCK_PAD,
                y: y + ERROR_BLOCK_PAD + ERROR_LINE_HEIGHT + 4.0,
                width: Some(CARD_WIDTH - PADDING_X * 2.0 - ERROR_BLOCK_PAD * 2.0),
                size: ERROR_DETAIL_SIZE,
                weight: 400,
                color: pal.ink4,
                font: super::card_font::FontKind::Mono,
                align: TextAlign::Left,
                role: LineRole::Plain,
            });
            y += block_height;
        }

        if let Some(proxy) = &plan.proxy {
            y += GROUP_GAP;
            // Full-width pill: health dot + "name · summary" + port.
            self.layers.box_container(
                PADDING_X,
                y,
                CARD_WIDTH - PADDING_X * 2.0,
                PROXY_ROW_HEIGHT,
                PROXY_ROW_HEIGHT / 2.0,
                if self.dark {
                    Rgba(1.0, 1.0, 1.0, 0.05)
                } else {
                    Rgba(0.0, 0.0, 0.0, 0.045)
                },
                None,
            );
            self.proxy_row = Some((y, y + PROXY_ROW_HEIGHT));

            let mid = y + PROXY_ROW_HEIGHT / 2.0;
            self.layers.dot(
                PADDING_X + 10.0,
                mid - 2.5,
                2.5,
                if proxy.ok {
                    Rgba(pal.ok.0, pal.ok.1, pal.ok.2, 0.85)
                } else {
                    Rgba(pal.danger.0, pal.danger.1, pal.danger.2, 0.85)
                },
            );

            let port = proxy_port(&proxy.url);
            let port_width = port.as_deref().map_or(0.0, |_| 44.0);
            // Short status like the prototype pill; the full summary lives
            // in the settings page's proxy tab one click away.
            let status = if !proxy.ok {
                "离线".to_string()
            } else if let Some(n) = proxy.accounts_available {
                format!("{n} 个账号可用")
            } else {
                "在线".to_string()
            };
            lines.push(TextSpec {
                text: format!("{} · {}", proxy.name, status),
                x: PADDING_X + 18.0,
                y: text_top(mid, PROXY_SIZE + 1.0),
                width: Some(CARD_WIDTH - PADDING_X * 2.0 - 24.0 - port_width - 6.0),
                size: PROXY_SIZE + 1.0,
                weight: 400,
                color: pal.ink2,
                font: super::card_font::FontKind::Ui,
                align: TextAlign::Left,
                role: LineRole::Plain,
            });
            if let Some(port) = port {
                lines.push(TextSpec {
                    text: port,
                    x: CARD_WIDTH - PADDING_X - 10.0 - port_width,
                    y: text_top(mid, PROXY_SIZE),
                    width: Some(port_width),
                    size: PROXY_SIZE,
                    weight: 400,
                    color: pal.ink4,
                    font: super::card_font::FontKind::Mono,
                    align: TextAlign::Right,
                    role: LineRole::Plain,
                });
            }

            y += PROXY_ROW_HEIGHT;
        }

        // Footer: freshness of the numbers; today's tokens (from the local
        // ledger, for tools that log them) or the pin badge on the right.
        y += 10.0;
        let (age_text, stale) = format_age(fetched_at, now_ms);
        let tokens_today = crate::token_ledger::today_for_plan(&plan.id).filter(|_| !pinned);
        let age_width = if tokens_today.is_some() {
            (CARD_WIDTH - PADDING_X * 2.0) / 2.0
        } else {
            CARD_WIDTH - PADDING_X * 2.0 - 48.0
        };
        if let Some(tokens) = tokens_today {
            lines.push(TextSpec {
                text: format!("今日 {} token", format_tokens(tokens)),
                x: CARD_WIDTH / 2.0,
                y,
                width: Some(CARD_WIDTH / 2.0 - PADDING_X),
                size: FOOTER_SIZE,
                weight: 400,
                color: pal.ink3,
                font: super::card_font::FontKind::Mono,
                align: TextAlign::Right,
                role: LineRole::Plain,
            });
        }
        lines.push(TextSpec {
            text: age_text,
            x: PADDING_X,
            y,
            width: Some(age_width),
            size: FOOTER_SIZE,
            weight: 400,
            color: if stale { pal.warn } else { pal.ink4 },
            font: super::card_font::FontKind::Mono,
            align: TextAlign::Left,
            role: LineRole::Freshness(fetched_at),
        });
        // Clicks poke the ball now; only the `--debug-card` pin still marks
        // itself so a screenshot shows why the card is held open.
        if pinned {
            lines.push(TextSpec {
                text: "已钉住".into(),
                x: CARD_WIDTH - PADDING_X - 110.0,
                y,
                width: Some(110.0),
                size: FOOTER_SIZE,
                weight: 600,
                color: plan_rgb,
                font: super::card_font::FontKind::Ui,
                align: TextAlign::Right,
                role: LineRole::Plain,
            });
        }
        y += FOOTER_SIZE * 1.4;

        let height = y + PADDING_BOTTOM;
        self.height = height;
        self.layers.layout(CARD_WIDTH, height);
        self.layers.render_texts(&lines);
        self.layers.set_visible(true);
        height
    }

    /// Refresh the ticking lines (countdowns + freshness) once a second —
    /// text updates only, no relayout. Countdown specs carry the full line
    /// ("最新口径 · 3h12m 后重置"), so the suffix is swapped while any
    /// leading prefix stays put.
    pub fn tick_content(&mut self, now_ms: u64) {
        let Some(specs) = self.layers.text_specs() else {
            return;
        };
        for (index, spec) in specs.iter().enumerate() {
            match spec.role {
                LineRole::Plain => {}
                LineRole::Countdown(at) => {
                    let fresh = format_reset(at, now_ms);
                    let text = match spec.text.rsplit_once(" · ") {
                        // Only swap when the tail still looks like a
                        // countdown; "待刷新" keeps the split too.
                        Some((head, tail)) if tail.ends_with("后重置") || tail == "待刷新" => {
                            format!("{head} · {fresh}")
                        }
                        _ => fresh,
                    };
                    if spec.text != text {
                        self.layers.update_line(index, &text, None);
                    }
                }
                LineRole::Freshness(at) => {
                    let (text, stale) = format_age(at, now_ms);
                    if spec.text != text {
                        let pal = card_palette(self.dark);
                        self.layers.update_line(
                            index,
                            &text,
                            Some(if stale { pal.warn } else { pal.ink4 }),
                        );
                    }
                }
            }
        }
    }

    /// Flip the layer to actually hidden after the fade-out animation finished.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.layers.set_hidden(hidden);
    }

    /// Point the speech tail at the hovered ball. Call after `show_plan`
    /// (which lays out the plain body) once the card's side is known.
    pub fn set_tail(&self, tail_left: bool, tail_y: f64) {
        self.layers
            .layout_with_tail(CARD_WIDTH, self.height, tail_left, tail_y);
    }

    pub fn hide(&mut self) {
        self.layers.set_visible(false);
        self.plan_id = None;
        self.proxy_row = None;
    }
}

/// Baseline-independent vertical centring: CATextLayer frames are laid from
/// the top, so text rows centre on the bar by hand.
fn row_mid(row_top: f64) -> f64 {
    row_top + ROW_HEIGHT / 2.0
}

fn text_top(row_mid_y: f64, size: f64) -> f64 {
    row_mid_y - size * 1.35 / 2.0
}

/// One group of buckets sharing the same label prefix before the last " · "
/// (e.g. "e***@g*** Gemini" or "standard"). `None` groups carry labels with
/// no separator and render without a heading.
#[derive(Debug)]
pub struct BucketGroup<'a> {
    pub name: Option<&'a str>,
    /// (original label, row display name, bucket)
    pub rows: Vec<(&'a str, &'a str, &'a Bucket)>,
}

/// Split a bucket label into its group prefix and row name at the LAST
/// " · ". No separator → anonymous group, the full label is the row name.
pub fn split_group(label: &str) -> (Option<&str>, &str) {
    match label.rfind(" · ") {
        Some(index) => {
            // 分隔符本身是 5 字节（空格 + '·' + 空格），行名从分隔符之后开始。
            let (head, tail) = label.split_at(index);
            (Some(head), &tail[" · ".len()..])
        }
        None => (None, label),
    }
}

/// Group buckets in first-appearance order of their label prefix.
pub fn group_buckets(buckets: &[Bucket]) -> Vec<BucketGroup<'_>> {
    let mut groups: Vec<BucketGroup<'_>> = Vec::new();
    for bucket in buckets {
        let (group_name, row_name) = split_group(bucket.label.as_str());
        match groups.iter_mut().find(|group| group.name == group_name) {
            Some(group) => group.rows.push((bucket.label.as_str(), row_name, bucket)),
            None => groups.push(BucketGroup {
                name: group_name,
                rows: vec![(bucket.label.as_str(), row_name, bucket)],
            }),
        }
    }
    groups
}

/// Numeric port between host and path, e.g. "http://127.0.0.1:8050/healthz"
/// → "8050". Returns `None` for URLs without an explicit port.
pub fn proxy_port(url: &str) -> Option<String> {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let (_, port) = authority.rsplit_once(':')?;
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(port.to_string())
}

/// Format the reset countdown exactly like `ui/common.js::formatReset`.
pub fn format_reset(resets_at: u64, now_ms: u64) -> String {
    if resets_at <= now_ms {
        return "待刷新".into();
    }
    let diff = resets_at - now_ms;
    const DAY: u64 = 86_400_000;
    const HOUR: u64 = 3_600_000;
    const MINUTE: u64 = 60_000;
    if diff >= DAY {
        format!("{}d {}h 后重置", diff / DAY, (diff % DAY) / HOUR)
    } else if diff < HOUR {
        format!("{}m 后重置", (diff / MINUTE).max(1))
    } else {
        format!("{}h{}m 后重置", diff / HOUR, (diff % HOUR) / MINUTE)
    }
}

/// Colour for bucket bars: the plan's own palette colour while healthy
/// (≥40 % — the ball's identity colour carries "fine"), warn/danger below
/// it. `plan_rgb` is the resolved palette colour for the plan.
pub fn metric_color(ok: bool, percent: Option<f64>, plan_rgb: Rgba, pal: &CardPalette) -> Rgba {
    if !ok {
        pal.danger
    } else if percent.is_none() {
        pal.ink4
    } else if percent.is_some_and(|p| p >= 40.0) {
        plan_rgb
    } else if percent.is_some_and(|p| p >= 15.0) {
        pal.warn
    } else {
        pal.danger
    }
}

/// Token counts the Chinese way: 1.66 亿, 363 万, 8,988.
fn format_tokens(n: u64) -> String {
    if n >= 100_000_000 {
        format!("{:.2} 亿", n as f64 / 1e8)
    } else if n >= 1_000_000 {
        format!("{:.0} 万", n as f64 / 1e4)
    } else if n >= 10_000 {
        format!("{:.1} 万", n as f64 / 1e4)
    } else {
        n.to_string()
    }
}

/// "Ns 前更新" for the footer. `fetched_at` is Unix seconds, `now_ms` Unix ms;
/// returns the text and whether the data counts as stale (>2× the 60 s loop).
pub fn format_age(fetched_at: u64, now_ms: u64) -> (String, bool) {
    if fetched_at == 0 {
        return ("数据尚未拉取".into(), true);
    }
    let age = (now_ms / 1000).saturating_sub(fetched_at);
    let stale = age > 130;
    let text = if age < 60 {
        format!("{age}s 前更新")
    } else {
        format!("{}m{}s 前更新", age / 60, age % 60)
    };
    (text, stale)
}

/// Colour for the headline percentage, matching `ui/common.js::colorFor`.
pub fn percent_color(plan: &PlanQuota, pal: &CardPalette) -> Rgba {
    percent_color_for(plan.ok, plan.remaining_percent, pal)
}

pub fn percent_color_for(ok: bool, percent: Option<f64>, pal: &CardPalette) -> Rgba {
    if !ok {
        pal.danger
    } else if percent.is_none() {
        pal.ink4
    } else if percent.is_some_and(|p| p >= 40.0) {
        pal.ok
    } else if percent.is_some_and(|p| p >= 15.0) {
        pal.warn
    } else {
        pal.danger
    }
}

/// Decide which side of the capsule the card opens on.
pub fn choose_side(
    capsule_x: f64,
    capsule_width: f64,
    card_width: f64,
    screen: (f64, f64, f64, f64),
) -> CardSide {
    let (_left, _top, screen_right, _bottom) = screen;
    let right_x = capsule_x + capsule_width + CARD_GAP;
    if right_x + card_width <= screen_right - SCREEN_MARGIN {
        CardSide::Right
    } else {
        CardSide::Left
    }
}

/// Card position in top-left origin logical coordinates.
pub fn card_position(
    capsule_x: f64,
    capsule_y: f64,
    capsule_width: f64,
    ball_center_y: f64,
    card_width: f64,
    card_height: f64,
    side: CardSide,
    screen: (f64, f64, f64, f64),
) -> (f64, f64) {
    let (screen_left, screen_top, screen_right, screen_bottom) = screen;
    let x = match side {
        CardSide::Right => capsule_x + capsule_width + CARD_GAP,
        CardSide::Left => (capsule_x - CARD_GAP - card_width).max(screen_left + SCREEN_MARGIN),
    };
    let mut y = capsule_y + ball_center_y - card_height / 2.0;
    let min_y = screen_top + SCREEN_MARGIN;
    let max_y = (screen_bottom - SCREEN_MARGIN - card_height).max(min_y);
    y = y.clamp(min_y, max_y);
    let _ = (screen_right, capsule_width);
    (x, y)
}

#[cfg(test)]
mod tests {

    #[test]
    fn token_counts_read_the_chinese_way() {
        assert_eq!(super::format_tokens(8_988), "8988");
        assert_eq!(super::format_tokens(12_345), "1.2 万");
        assert_eq!(super::format_tokens(3_630_000), "363 万");
        assert_eq!(super::format_tokens(166_000_000), "1.66 亿");
    }

    use super::*;

    fn bucket(label: &str) -> Bucket {
        Bucket {
            label: label.into(),
            remaining_percent: Some(50.0),
            detail: None,
            resets_at: None,
            pool: None,
            window: None,
        }
    }

    #[test]
    fn countdown_formats_match_the_web_ui() {
        let now = 1_700_000_000_000;
        assert_eq!(
            format_reset(now + 2 * 86_400_000 + 3 * 3_600_000, now),
            "2d 3h 后重置"
        );
        assert_eq!(
            format_reset(now + 5 * 3_600_000 + 30 * 60_000, now),
            "5h30m 后重置"
        );
        assert_eq!(format_reset(now + 42 * 60_000, now), "42m 后重置");
        assert_eq!(format_reset(now - 1, now), "待刷新");
        // Under a minute still shows 1m rather than 0m.
        assert_eq!(format_reset(now + 10_000, now), "1m 后重置");
    }

    // split_group / group_buckets 吃五家真实标签。

    #[test]
    fn split_group_takes_the_last_separator() {
        assert_eq!(split_group("5 小时"), (None, "5 小时"));
        assert_eq!(split_group("周额度"), (None, "周额度"));
        assert_eq!(
            split_group("e***@g*** Gemini · 周"),
            (Some("e***@g*** Gemini"), "周")
        );
        assert_eq!(
            split_group("e***@g*** Claude & GPT · 周"),
            (Some("e***@g*** Claude & GPT"), "周")
        );
        assert_eq!(split_group("standard · 周"), (Some("standard"), "周"));
        assert_eq!(split_group("core · 5 小时"), (Some("core"), "5 小时"));
        // 全角点不误拆；末尾空格不进组名。
        assert_eq!(split_group("Fast requests"), (None, "Fast requests"));
    }

    #[test]
    fn group_buckets_claude_stays_anonymous() {
        let buckets = [bucket("5 小时"), bucket("7 天")];
        let groups = group_buckets(&buckets);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, None);
        assert_eq!(groups[0].rows.len(), 2);
        assert_eq!(groups[0].rows[0].1, "5 小时");
    }

    #[test]
    fn group_buckets_vendor_credits_land_in_the_anonymous_group() {
        let buckets = [
            bucket("周额度"),
            bucket("5 小时"),
            bucket("Cloud agents"),
            bucket("Credits"),
        ];
        let groups = group_buckets(&buckets);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, None);
        assert_eq!(groups[0].rows.len(), 4);
    }

    #[test]
    fn group_buckets_antigravity_separates_accounts_and_pools() {
        let labels = [
            "e***@g*** Gemini · 周",
            "e***@g*** Gemini · 5 小时",
            "e***@g*** Claude & GPT · 周",
            "e***@g*** Claude & GPT · 5 小时",
            "o***@g*** Gemini · 周",
            "o***@g*** Gemini · 5 小时",
            "o***@g*** Claude & GPT · 周",
            "o***@g*** Claude & GPT · 5 小时",
        ];
        let buckets: Vec<_> = labels.iter().map(|label| bucket(label)).collect();
        let groups = group_buckets(&buckets);
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].name, Some("e***@g*** Gemini"));
        assert_eq!(groups[1].name, Some("e***@g*** Claude & GPT"));
        assert_eq!(groups[2].name, Some("o***@g*** Gemini"));
        assert_eq!(groups[3].name, Some("o***@g*** Claude & GPT"));
        assert!(groups.iter().all(|group| group.rows.len() == 2));
        assert_eq!(groups[0].rows[0].1, "周");
        assert_eq!(groups[0].rows[1].1, "5 小时");
    }

    #[test]
    fn group_buckets_cursor_has_no_groups() {
        let labels = ["总额度", "Fast requests", "API", "Grok Bot"];
        let buckets: Vec<_> = labels.iter().map(|label| bucket(label)).collect();
        let groups = group_buckets(&buckets);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, None);
        assert_eq!(groups[0].rows.len(), 4);
    }

    #[test]
    fn group_buckets_factory_splits_pools_in_first_seen_order() {
        let labels = [
            "standard · 周",
            "standard · 月",
            "standard · 5 小时",
            "core · 周",
            "core · 月",
            "core · 5 小时",
        ];
        let buckets: Vec<_> = labels.iter().map(|label| bucket(label)).collect();
        let groups = group_buckets(&buckets);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, Some("standard"));
        assert_eq!(groups[1].name, Some("core"));
        assert_eq!(groups[0].rows.len(), 3);
        assert_eq!(groups[1].rows.len(), 3);
        // First-appearance order, not alphabetical.
        let labels = ["core · 周", "standard · 周"];
        let buckets: Vec<_> = labels.iter().map(|label| bucket(label)).collect();
        let groups = group_buckets(&buckets);
        assert_eq!(groups[0].name, Some("core"));
        assert_eq!(groups[1].name, Some("standard"));
    }

    #[test]
    fn proxy_port_parses_the_loopback_urls() {
        assert_eq!(
            proxy_port("http://127.0.0.1:8050/healthz"),
            Some("8050".to_string())
        );
        assert_eq!(
            proxy_port("http://127.0.0.1:8050/"),
            Some("8050".to_string())
        );
        assert_eq!(proxy_port("https://api.example.com/usage"), None);
        assert_eq!(proxy_port("http://localhost/healthz"), None);
        assert_eq!(proxy_port(""), None);
    }

    #[test]
    fn side_flips_when_the_capsule_is_near_the_right_edge() {
        let screen = (0.0, 0.0, 1440.0, 900.0);
        assert_eq!(
            choose_side(1000.0, 62.0, CARD_WIDTH, screen),
            CardSide::Right
        );
        assert_eq!(
            choose_side(1200.0, 62.0, CARD_WIDTH, screen),
            CardSide::Left
        );
    }

    #[test]
    fn card_position_clamps_inside_the_screen() {
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let (x, y) = card_position(
            1000.0,
            400.0,
            62.0,
            150.0,
            CARD_WIDTH,
            200.0,
            CardSide::Right,
            screen,
        );
        assert_eq!(x, 1070.0);
        // Vertically centred on the hovered ball.
        assert!((y - 450.0).abs() < 1e-9);

        // Very low on screen: clamp to the bottom margin.
        let (_x, y) = card_position(
            1000.0,
            850.0,
            62.0,
            290.0,
            CARD_WIDTH,
            200.0,
            CardSide::Right,
            screen,
        );
        assert_eq!(y, 900.0 - SCREEN_MARGIN - 200.0);
    }

    #[test]
    fn card_position_on_the_left_keeps_the_screen_margin() {
        let screen = (0.0, 0.0, 1440.0, 900.0);
        let (x, _y) = card_position(
            20.0,
            400.0,
            62.0,
            150.0,
            CARD_WIDTH,
            200.0,
            CardSide::Left,
            screen,
        );
        assert_eq!(x, 8.0);
    }

    #[test]
    fn percent_colours_follow_the_web_thresholds() {
        let pal = card_palette(true);
        assert_eq!(percent_color_for(false, None, &pal), pal.danger);
        assert_eq!(percent_color_for(true, None, &pal), pal.ink4);
        assert_eq!(percent_color_for(true, Some(60.0), &pal), pal.ok);
        assert_eq!(percent_color_for(true, Some(39.9), &pal), pal.warn);
        assert_eq!(percent_color_for(true, Some(14.9), &pal), pal.danger);
    }
}
