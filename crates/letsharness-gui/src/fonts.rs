//! システムフォントを太さ違いでも使えるようにする。
//!
//! macOS の SF (`System Font`) と SF Mono (`.SF NS Mono`) は可変フォントで、fontdb には既定の太さ
//! (SF は 400、SF Mono は 295) の face 1つとしてしか登録されない。cosmic-text は太さが完全に一致する
//! face しか選ばないため、そのままでは太字が別のフォント (Menlo Bold) に、SF Mono は 400 で選ばれずに
//! フォールバックしてしまう。
//!
//! そこで同じ face を 100〜900 の各太さでも登録しておく。描画時は cosmic-text が要求された太さを
//! `wght` 軸に指定するので、本物の SF の太字・SF Mono として描かれる。

/// 太さ違いを登録する可変フォントの family 名。
#[cfg(target_os = "macos")]
const VARIABLE_FAMILIES: [&str; 2] = ["System Font", ".SF NS Mono"];

/// テキストを描く前 (アプリ起動前) に呼ぶ。
pub fn register_system_font_weights() {
	#[cfg(target_os = "macos")]
	{
		use iced::advanced::graphics::text::cosmic_text::fontdb::{FaceInfo, Weight};
		use iced::advanced::graphics::text::font_system;

		let Ok(mut system) = font_system().write() else {
			return;
		};
		let db = system.raw().db_mut();
		let faces: Vec<FaceInfo> = db
			.faces()
			.filter(|face| {
				face.families
					.iter()
					.any(|(name, _)| VARIABLE_FAMILIES.contains(&name.as_str()))
			})
			.cloned()
			.collect();
		for face in faces {
			for weight in (100..=900).step_by(100).filter(|w| *w != face.weight.0) {
				db.push_face_info(FaceInfo {
					weight: Weight(weight),
					..face.clone()
				});
			}
		}
	}
}
