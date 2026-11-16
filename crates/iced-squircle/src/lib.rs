//! iced で Squircle (超楕円の角) の面を描くライブラリ。
//!
//! iced の `border.radius` は円弧の角しか描けないため、canvas で超楕円のパスを描いて中身の下に敷く。
//!
//! ```no_run
//! use iced::widget::text;
//! use iced::{Color, Element};
//! use iced_squircle::{Surface, squircle};
//!
//! const CARD: Surface = Surface {
//! 	radius: 12.0,
//! 	fill: Color::WHITE,
//! 	border: Some(Color::BLACK),
//! 	shadow: None,
//! };
//!
//! fn view<'a, Message: 'a>() -> Element<'a, Message> {
//! 	squircle(text("Hello"), CARD)
//! }
//! ```

use std::f32::consts::{FRAC_PI_2, PI};

use iced::widget::canvas::{self, Cache, Geometry, Path, Stroke};
use iced::widget::{Stack, container};
use iced::{Color, Element, Fill, Point, Rectangle, Renderer, Shadow, Theme, mouse};

/// 角の丸みの指数。2 で円、大きいほど四角に近い。
const EXPONENT: f32 = 5.0;
/// 超楕円の角の大きさ / 同じ深さに見える円弧の半径。
///
/// 対角線方向の削れ具合は円弧が `0.414 r`、指数 5 の超楕円が `0.184 R` なので、`R = 2.25 r` で揃う。
const EXTENT: f32 = 2.25;
/// 角1つあたりの分割数。
const STEPS: usize = 24;

/// Squircle の面の見た目。
#[derive(Debug, Clone, Copy)]
pub struct Surface {
	/// 角の丸み。`border.radius` と同じ感覚で指定できるよう、対角線方向の削れ具合が
	/// この半径の円弧と同じになる大きさで描く (曲がり始めは円弧より手前になる)。
	pub radius: f32,
	pub fill: Color,
	pub border: Option<Color>,
	pub shadow: Option<Shadow>,
}

/// `content` の下に squircle の面を敷く。大きさは `content` のまま。
pub fn squircle<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, surface: Surface) -> Element<'a, Message> {
	let content = content.into();
	let size = content.as_widget().size_hint();
	let shape = canvas::Canvas::new(Shape(surface)).width(Fill).height(Fill);
	let layered = Stack::with_children([content])
		.width(size.width)
		.height(size.height)
		.push_under(shape);

	let Some(shadow) = surface.shadow else {
		return layered.into();
	};
	// canvas ではぼかせないので、影は同じ丸みの円弧の角の影で代用する (ぼけるので形の差はほぼ見えない)。
	container(layered)
		.style(move |_| container::Style {
			border: iced::border::rounded(surface.radius),
			shadow,
			..container::Style::default()
		})
		.into()
}

struct Shape(Surface);

impl<Message> canvas::Program<Message> for Shape {
	type State = Cache;

	fn draw(
		&self,
		cache: &Cache,
		renderer: &Renderer,
		_theme: &Theme,
		bounds: Rectangle,
		_cursor: mouse::Cursor,
	) -> Vec<Geometry> {
		let surface = self.0;
		vec![cache.draw(renderer, bounds.size(), |frame| {
			// 枠線は線の中心がパス上に来るので、半分だけ内側に寄せてはみ出さないようにする。
			let inset = if surface.border.is_some() { 0.5 } else { 0.0 };
			let rect = Rectangle {
				x: inset,
				y: inset,
				width: bounds.width - inset * 2.0,
				height: bounds.height - inset * 2.0,
			};
			let path = path(rect, surface.radius);
			frame.fill(&path, surface.fill);
			if let Some(border) = surface.border {
				frame.stroke(&path, Stroke::default().with_color(border).with_width(1.0));
			}
		})]
	}
}

/// `rect` を丸み `radius` ([`Surface::radius`]) の squircle にしたパス。
///
/// canvas で自前に描きたいとき用。
pub fn path(rect: Rectangle, radius: f32) -> Path {
	let r = (radius * EXTENT).min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
	let (left, top) = (rect.x + r, rect.y + r);
	let (right, bottom) = (rect.x + rect.width - r, rect.y + rect.height - r);
	// 右上から時計回り。各角は、その角の中心から見た超楕円の 1/4。
	let corners = [
		(Point::new(right, top), -FRAC_PI_2),
		(Point::new(right, bottom), 0.0),
		(Point::new(left, bottom), FRAC_PI_2),
		(Point::new(left, top), PI),
	];
	let power = 2.0 / EXPONENT;

	Path::new(|p| {
		for (i, (center, start)) in corners.into_iter().enumerate() {
			for step in 0..=STEPS {
				let t = start + FRAC_PI_2 * step as f32 / STEPS as f32;
				let (sin, cos) = t.sin_cos();
				let point = Point::new(
					center.x + r * cos.signum() * cos.abs().powf(power),
					center.y + r * sin.signum() * sin.abs().powf(power),
				);
				if i == 0 && step == 0 {
					p.move_to(point);
				} else {
					p.line_to(point);
				}
			}
		}
		p.close();
	})
}
