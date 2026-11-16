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
//!     radius: 12.0,
//!     fill: Color::WHITE,
//!     border: Some(Color::BLACK),
//!     shadow: None,
//! };
//!
//! fn view<'a, Message: 'a>() -> Element<'a, Message> {
//!     squircle(text("Hello"), CARD)
//! }
//! ```

use std::f32::consts::FRAC_PI_2;
use std::sync::LazyLock;

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
	/// 実際の角の大きさは [`path`] を参照。
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
///
/// `radius` は角の大きさそのものではない。同じ深さに見える円弧の半径で、実際に曲がり始める位置は
/// 角から `radius * 2.25` (幅と高さの半分で頭打ち) になる。`radius` が有限でないときは 0 として扱う
/// (debug ビルドでは panic する)。
///
/// 毎回パスを組み直すので、毎フレーム描くなら [`canvas::Cache`] に乗せること。
pub fn path(rect: Rectangle, radius: f32) -> Path {
	let points = outline(rect, radius);
	if let [first, rest @ ..] = points.as_slice()
		&& !rest.is_empty()
	{
		Path::new(|p| {
			p.move_to(*first);
			for point in rest {
				p.line_to(*point);
			}
			p.close();
		})
	} else {
		Path::rectangle(rect.position(), rect.size())
	}
}

/// [`path`] の輪郭の点。右上の角の始まりから時計回りで、角が無ければ空。
fn outline(rect: Rectangle, radius: f32) -> Vec<Point> {
	debug_assert!(radius.is_finite(), "squircle radius must be finite, got {radius}");
	let radius = if radius.is_finite() { radius } else { 0.0 };
	let r = (radius * EXTENT).min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
	if r == 0.0 {
		return Vec::new();
	}
	let (left, top) = (rect.x + r, rect.y + r);
	let (right, bottom) = (rect.x + rect.width - r, rect.y + rect.height - r);
	// 各角は単位 1/4 曲線を符号で向きを変えて `r` 倍したもの。曲線は x 軸側から y 軸側へ進むので、
	// 時計回りにするには右上と左下では逆に辿る。
	let corners = [
		(Point::new(right, top), (1.0, -1.0), true),
		(Point::new(right, bottom), (1.0, 1.0), false),
		(Point::new(left, bottom), (-1.0, 1.0), true),
		(Point::new(left, top), (-1.0, -1.0), false),
	];

	let quarter = &*QUARTER;
	let mut points = Vec::with_capacity(corners.len() * quarter.len());
	for (center, (sx, sy), reversed) in corners {
		for step in 0..quarter.len() {
			let (x, y) = quarter[if reversed { quarter.len() - 1 - step } else { step }];
			let point = Point::new(center.x + sx * r * x, center.y + sy * r * y);
			// 辺の長さが 0 のとき (ピル形) は隣の角の端と重なるので、同じ点を打たない。
			if points.last() != Some(&point) {
				points.push(point);
			}
		}
	}
	points
}

/// 第1象限の単位超楕円 `x^n + y^n = 1` を `(1, 0)` から `(0, 1)` まで辿った点。
///
/// `θ ∈ [0, π/2]` で `(cos^(2/n), sin^(2/n))`。`cos(π/2)` が f32 では僅かに負になるので 0 で潰す。
static QUARTER: LazyLock<[(f32, f32); STEPS + 1]> = LazyLock::new(|| {
	let power = 2.0 / EXPONENT;
	std::array::from_fn(|step| {
		let (sin, cos) = (FRAC_PI_2 * step as f32 / STEPS as f32).sin_cos();
		(cos.max(0.0).powf(power), sin.max(0.0).powf(power))
	})
});

#[cfg(test)]
mod tests {
	use super::*;

	const RECT: Rectangle = Rectangle {
		x: 10.0,
		y: 20.0,
		width: 200.0,
		height: 100.0,
	};

	#[test]
	fn quarter_ends_exactly_on_the_axes() {
		assert_eq!(QUARTER[0], (1.0, 0.0));
		assert_eq!(QUARTER[STEPS], (0.0, 1.0));
		assert!(QUARTER.iter().all(|&(x, y)| x >= 0.0 && y >= 0.0));
	}

	#[test]
	fn corners_are_mirror_symmetric() {
		// 原点中心なら鏡映しは符号の反転になり、丸め誤差も対称に乗るので厳密に比べられる。
		let rect = Rectangle {
			x: -100.0,
			y: -50.0,
			width: 200.0,
			height: 100.0,
		};
		let points = outline(rect, 8.0);
		let n = STEPS + 1;
		assert_eq!(points.len(), n * 4);
		for (i, p) in points[..n].iter().enumerate() {
			// 右上 ↔ 右下は上下に、右上 ↔ 左上は左右に鏡映し (どちらも辿る向きは逆)。
			assert_eq!(points[2 * n - 1 - i], Point::new(p.x, -p.y));
			assert_eq!(points[4 * n - 1 - i], Point::new(-p.x, p.y));
			// 左下は点対称で、辿る向きは同じ。
			assert_eq!(points[2 * n + i], Point::new(-p.x, -p.y));
		}
	}

	#[test]
	fn radius_is_clamped_to_half_the_shorter_side() {
		let points = outline(RECT, 1000.0);
		let top = points.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
		let bottom = points.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
		assert_eq!((top, bottom), (RECT.y, RECT.y + RECT.height));
		// 左右の辺は長さ 0 なので、角どうしの継ぎ目の点が重複しない。
		assert!(points.windows(2).all(|w| w[0] != w[1]));
	}

	#[test]
	fn zero_radius_has_no_corners() {
		assert!(outline(RECT, 0.0).is_empty());
		assert!(outline(RECT, -3.0).is_empty());
	}

	#[test]
	#[cfg_attr(debug_assertions, should_panic(expected = "finite"))]
	fn nan_radius_is_treated_as_zero() {
		assert!(outline(RECT, f32::NAN).is_empty());
	}
}
