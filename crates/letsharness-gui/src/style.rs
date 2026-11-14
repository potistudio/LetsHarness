//! Claude 風の配色とウィジェットのスタイル。

use iced::widget::{button, column, container, markdown, pick_list, rich_text, scrollable, text, text_editor};
use iced::{Background, Border, Color, Element, Fill, Font, Shadow, Theme, Vector, border, color, font, theme};

pub const BG: Color = color!(0xFAF9F5);
pub const SIDEBAR: Color = color!(0xF5F4ED);
pub const SURFACE: Color = color!(0xFFFFFF);
pub const BUBBLE: Color = color!(0xF0EEE6);
pub const HOVER: Color = color!(0xEAE7DD);
pub const BORDER: Color = color!(0xE3E0D5);
pub const TEXT: Color = color!(0x29261B);
pub const MUTED: Color = color!(0x77756C);
pub const ACCENT: Color = color!(0xD97757);
pub const ACCENT_HOVER: Color = color!(0xC6613F);
pub const DANGER: Color = color!(0xB53333);

pub const SERIF: Font = Font {
	family: font::Family::Serif,
	..Font::DEFAULT
};

pub fn theme() -> Theme {
	Theme::custom(
		"Claude",
		theme::Palette {
			background: BG,
			text: TEXT,
			primary: ACCENT,
			success: color!(0x5A8A5A),
			warning: color!(0xC98A2B),
			danger: DANGER,
		},
	)
}

fn fill(color: Color, radius: f32) -> container::Style {
	container::Style {
		background: Some(color.into()),
		border: border::rounded(radius),
		..container::Style::default()
	}
}

pub fn sidebar(_: &Theme) -> container::Style {
	fill(SIDEBAR, 0.0)
}

pub fn divider(_: &Theme) -> container::Style {
	fill(BORDER, 0.0)
}

pub fn user_bubble(_: &Theme) -> container::Style {
	fill(BUBBLE, 14.0)
}

pub fn thinking(_: &Theme) -> container::Style {
	container::Style {
		text_color: Some(MUTED),
		border: Border {
			color: BORDER,
			width: 1.0,
			radius: 10.0.into(),
		},
		..container::Style::default()
	}
}

pub fn composer(_: &Theme) -> container::Style {
	container::Style {
		background: Some(SURFACE.into()),
		border: Border {
			color: BORDER,
			width: 1.0,
			radius: 18.0.into(),
		},
		shadow: Shadow {
			color: Color::from_rgba8(0, 0, 0, 0.06),
			offset: Vector::new(0.0, 2.0),
			blur_radius: 12.0,
		},
		..container::Style::default()
	}
}

pub fn editor(_: &Theme, _: text_editor::Status) -> text_editor::Style {
	text_editor::Style {
		background: Background::Color(Color::TRANSPARENT),
		border: Border::default(),
		placeholder: MUTED,
		value: TEXT,
		selection: Color { a: 0.35, ..ACCENT },
	}
}

/// サイドバーの項目。`selected` は開いている会話。
pub fn nav(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
	move |_, status| {
		let background = match status {
			_ if selected => Some(HOVER.into()),
			button::Status::Hovered | button::Status::Pressed => Some(BUBBLE.into()),
			_ => None,
		};
		button::Style {
			background,
			text_color: TEXT,
			border: border::rounded(8),
			..button::Style::default()
		}
	}
}

/// 文字だけの控えめなボタン (Thinking の開閉など)。
pub fn ghost(_: &Theme, status: button::Status) -> button::Style {
	button::Style {
		background: matches!(status, button::Status::Hovered).then(|| BUBBLE.into()),
		text_color: MUTED,
		border: border::rounded(6),
		..button::Style::default()
	}
}

/// 送信 (丸いアクセント色) / 停止ボタン。
pub fn round(color: Color, hover: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
	move |_, status| {
		let background = match status {
			button::Status::Hovered | button::Status::Pressed => hover,
			button::Status::Disabled => Color { a: 0.4, ..color },
			button::Status::Active => color,
		};
		button::Style {
			background: Some(background.into()),
			text_color: Color::WHITE,
			border: border::rounded(10),
			..button::Style::default()
		}
	}
}

pub fn picker(_: &Theme, status: pick_list::Status) -> pick_list::Style {
	let hovered = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
	pick_list::Style {
		text_color: MUTED,
		placeholder_color: MUTED,
		handle_color: MUTED,
		background: if hovered { BUBBLE } else { Color::TRANSPARENT }.into(),
		border: border::rounded(8),
	}
}

pub fn markdown_settings() -> markdown::Settings {
	let mut style = markdown::Style::from_palette(theme().palette());
	style.inline_code_highlight = markdown::Highlight {
		background: BUBBLE.into(),
		border: border::rounded(4),
	};
	style.inline_code_color = TEXT;
	let mut settings = markdown::Settings::with_text_size(15, style);
	settings.code_size = 13.into();
	settings
}

fn code_block(_: &Theme) -> container::Style {
	container::Style {
		background: Some(color!(0xF3F1EA).into()),
		border: Border {
			color: BORDER,
			width: 1.0,
			radius: 10.0.into(),
		},
		..container::Style::default()
	}
}

/// 既定の黒いコードブロックを、淡い背景 + 言語名のラベルに差し替える Markdown の描画。
pub struct MarkdownViewer;

impl<'a> markdown::Viewer<'a, markdown::Uri> for MarkdownViewer {
	fn on_link_click(url: markdown::Uri) -> markdown::Uri {
		url
	}

	fn code_block(
		&self,
		settings: markdown::Settings,
		language: Option<&'a str>,
		_code: &'a str,
		lines: &'a [markdown::Text],
	) -> Element<'a, markdown::Uri> {
		let code = column(lines.iter().map(|line| {
			rich_text(line.spans(settings.style))
				.on_link_click(Self::on_link_click)
				.font(settings.style.code_block_font)
				.size(settings.code_size)
				.into()
		}));
		let code = scrollable(container(code).padding([4, 14])).direction(scrollable::Direction::Horizontal(
			scrollable::Scrollbar::default().width(4).scroller_width(4),
		));
		let label = language.filter(|l| !l.is_empty()).unwrap_or("code");
		container(column![container(text(label).size(12).color(MUTED)).padding([0, 14]), code].spacing(6))
			.padding([10, 0])
			.width(Fill)
			.style(code_block)
			.into()
	}
}
