use crate::events::inbound;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use base64::Engine as _;
use elgato_streamdeck::{
	AsyncStreamDeck, DeviceStateUpdate,
	images::{ImageRect, convert_image_with_format_async},
	info::Kind,
};
use image::GenericImageView as _;
use tokio::sync::RwLock;

static ELGATO_DEVICES: LazyLock<RwLock<HashMap<String, AsyncStreamDeck>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static LCD_FRAMEBUFFERS: LazyLock<RwLock<HashMap<String, image::RgbImage>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static HIDAPI: LazyLock<RwLock<Option<Arc<hidapi::HidApi>>>> = LazyLock::new(|| RwLock::new(None));

fn plusxl_lcd_strip(kind: Kind) -> Option<(u32, u32)> {
	if kind == Kind::PlusXl {
		Some((100, 1200))
	} else {
		None
	}
}

async fn flush_lcd_framebuffer(device: &AsyncStreamDeck, id: &str) -> Result<(), anyhow::Error> {
	let fbs = LCD_FRAMEBUFFERS.read().await;
	if let Some(fb) = fbs.get(id) {
		let format = device.kind().lcd_image_format().unwrap();
		let rotated = image::DynamicImage::ImageRgb8(fb.clone()).rotate270();
		let jpeg_data = convert_image_with_format_async(format, rotated)?;
		device.write_lcd_fill(&jpeg_data).await?;
	}
	Ok(())
}

/// Extract the average colour from an image.
fn extract_average_colour(img: &image::DynamicImage) -> (u8, u8, u8) {
	let (r_sum, g_sum, b_sum) = img
		.pixels()
		.fold((0u64, 0u64, 0u64), |(r, g, b), (_, _, pixel)| (r + pixel[0] as u64, g + pixel[1] as u64, b + pixel[2] as u64));
	let count = (img.width() * img.height()).max(1) as u64;
	((r_sum / count) as u8, (g_sum / count) as u8, (b_sum / count) as u8)
}

pub async fn update_image(context: &crate::shared::Context, image: Option<&str>) -> Result<(), anyhow::Error> {
	if let Some(device) = ELGATO_DEVICES.read().await.get(&context.device) {
		let kind = device.kind();
		if !kind.is_visual() {
			return Ok(());
		}
		let key_count = kind.key_count();
		let is_touch_point = context.controller == "Keypad" && context.position >= key_count;

		if let Some(image) = image {
			let data = image.split_once(',').unwrap().1;
			let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
			if context.controller == "Encoder" {
				let img = image::load_from_memory(&bytes)?;
				let seg_w = 200u32;
				let seg_h = 100u32;
				let final_img = if img.width() == seg_w && img.height() == seg_h {
					img
				} else {
					img.resize_exact(seg_w, seg_h, image::imageops::FilterType::Lanczos3)
				};
				if plusxl_lcd_strip(kind).is_some() {
					let seg = final_img.to_rgb8();
					let enc_idx = context.position as u32;
					let mut fbs = LCD_FRAMEBUFFERS.write().await;
					let fb = fbs.get_mut(&context.device).unwrap();
					let src = image::DynamicImage::ImageRgb8(seg).rotate90().to_rgb8();
					image::imageops::overlay(fb, &src, 0, (enc_idx * 200) as i64);
					drop(fbs);
					flush_lcd_framebuffer(device, &context.device).await?;
				} else {
					device.write_lcd(context.position as u16 * 200, 0, &ImageRect::from_image_async(final_img)?).await?;
				}
			} else if is_touch_point {
				let (r, g, b) = extract_average_colour(&image::load_from_memory(&bytes)?);
				device.set_touchpoint_color(context.position - key_count, r, g, b).await?;
			} else {
				device.set_button_image(context.position, image::load_from_memory(&bytes)?).await?;
			}
		} else if context.controller == "Encoder" {
			if plusxl_lcd_strip(kind).is_some() {
				let enc_idx = context.position as u32;
				let mut fbs = LCD_FRAMEBUFFERS.write().await;
				let fb = fbs.get_mut(&context.device).unwrap();
				for y in (enc_idx * 200)..((enc_idx + 1) * 200).min(1200) {
					for x in 0..100u32 {
						fb.put_pixel(x, y, image::Rgb([0, 0, 0]));
					}
				}
				drop(fbs);
				flush_lcd_framebuffer(device, &context.device).await?;
			} else {
				device
					.write_lcd(context.position as u16 * 200, 0, &ImageRect::from_image_async(image::DynamicImage::new_rgb8(200, 100))?)
					.await?;
			}
		} else if is_touch_point {
			device.set_touchpoint_color(context.position - key_count, 0, 0, 0).await?;
		} else {
			device.clear_button_image(context.position).await?;
		}
		device.flush().await?;
	}
	Ok(())
}

/// Clear all touchpoint LEDs on a device by setting them to black.
async fn clear_all_touchpoints(device: &AsyncStreamDeck) {
	for i in 0..device.kind().touchpoint_count() {
		let _ = device.set_touchpoint_color(i, 0, 0, 0).await;
	}
}

pub async fn clear_screen(id: &str) -> Result<(), anyhow::Error> {
	if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
		let kind = device.kind();
		device.clear_all_button_images().await?;
		if matches!(kind, Kind::Plus | Kind::PlusXl) {
			if plusxl_lcd_strip(kind).is_some() {
				let mut fbs = LCD_FRAMEBUFFERS.write().await;
				fbs.insert(id.to_string(), image::RgbImage::new(100, 1200));
				drop(fbs);
				flush_lcd_framebuffer(device, id).await?;
			} else {
				let lcd_w = kind.encoder_count() as u16 * 200;
				let blank = ImageRect {
					w: lcd_w, h: 100,
					data: {
						use image::codecs::jpeg::JpegEncoder;
						use image::ColorType;
						let mut v = Vec::new();
						JpegEncoder::new_with_quality(&mut v, 90).encode(&vec![0u8; lcd_w as usize * 100 * 3], lcd_w as u32, 100, ColorType::Rgb8.into()).unwrap();
						v
					}
				};
				device.write_lcd_fill(&blank.data).await?;
			}
		}
		clear_all_touchpoints(device).await;
		device.flush().await?;
	}
	Ok(())
}

pub async fn set_brightness(id: &str, brightness: u8) {
	if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
		let _ = device.set_brightness(brightness.clamp(0, 100)).await;
		let _ = device.flush().await;
	}
}

pub async fn reset_devices() {
	for (_id, device) in ELGATO_DEVICES.read().await.iter() {
		let _ = device.reset().await;
		let _ = device.flush().await;
	}
}

async fn init(device: AsyncStreamDeck, device_id: String) {
	if ELGATO_DEVICES.read().await.contains_key(&device_id) {
		return;
	}

	let kind = device.kind();
	let device_type = match kind {
		Kind::Original | Kind::OriginalV2 | Kind::Mk2 | Kind::Mk2Scissor | Kind::Mk2Module => 0,
		Kind::Mini | Kind::MiniMk2 | Kind::MiniDiscord | Kind::MiniMk2Module => 1,
		Kind::Xl | Kind::XlV2 | Kind::XlV2Module => 2,
		Kind::Pedal => 5,
		Kind::Plus | Kind::PlusXl => 7,
		Kind::Neo => 9,
	};
	let _ = device.clear_all_button_images().await;
	clear_all_touchpoints(&device).await;
	if plusxl_lcd_strip(kind).is_some() {
		LCD_FRAMEBUFFERS.write().await.insert(device_id.clone(), image::RgbImage::new(100, 1200));
	}
	if let Ok(settings) = crate::store::get_settings() {
		let _ = device.set_brightness(settings.value.brightness).await;
	}
	let _ = device.flush().await;
	crate::events::inbound::devices::register_device(
		"",
		crate::events::inbound::PayloadEvent {
			payload: crate::shared::DeviceInfo {
				id: device_id.clone(),
				plugin: String::new(),
				name: device.product().await.unwrap(),
				rows: kind.row_count(),
				columns: kind.column_count(),
				encoders: kind.encoder_count(),
				touchpoints: kind.touchpoint_count(),
				r#type: device_type,
			},
		},
	)
	.await
	.unwrap();

	let reader = device.get_reader();
	ELGATO_DEVICES.write().await.insert(device_id.clone(), device);
	let press = |position| inbound::PayloadEvent {
		payload: inbound::devices::PressPayload { device: device_id.clone(), position },
	};
	let encoder = |position, ticks: i8| inbound::PayloadEvent {
		payload: inbound::devices::TicksPayload {
			device: device_id.clone(),
			position,
			ticks: ticks.into(),
		},
	};
	// Each encoder slot is 200px wide on the touch strip
	let lcd_w = kind.lcd_strip_size().map(|(w, _)| w as u16).unwrap_or(800);
	let encoder_width: u16 = if kind.encoder_count() > 0 { lcd_w / kind.encoder_count() as u16 } else { 200 };
	let touch = |x: u16, y: u16, hold: bool| {
		let slot = (x / encoder_width).min(kind.encoder_count().saturating_sub(1) as u16) as u8;
		let local_x = x - (slot as u16 * encoder_width);
		inbound::PayloadEvent {
			payload: inbound::devices::TouchPayload {
				device: device_id.clone(),
				position: slot,
				tap_pos: (local_x, y),
				hold,
			},
		}
	};
	loop {
		let updates = match reader.read(100.0).await {
			Ok(updates) => updates,
			Err(_) => break,
		};
		for update in updates {
			{
				use std::time::SystemTime;
				let ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();
				match &update {
					DeviceStateUpdate::TouchScreenPress(x, y) => log::info!("[touch] t={} PRESS x={} y={} enc={}", ms, x, y, x / 200),
					DeviceStateUpdate::TouchScreenLongPress(x, y) => log::info!("[touch] t={} LONGPRESS x={} y={} enc={}", ms, x, y, x / 200),
					DeviceStateUpdate::TouchScreenSwipe(from, to) => log::info!(
						"[touch] t={} SWIPE from=({},{}) to=({},{}) enc={}->{}  dx={} dy={}",
						ms,
						from.0,
						from.1,
						to.0,
						to.1,
						from.0 / 200,
						to.0 / 200,
						(to.0 as i32) - (from.0 as i32),
						(to.1 as i32) - (from.1 as i32)
					),
					DeviceStateUpdate::EncoderDown(dial) => log::info!("[touch] t={} ENC_DOWN dial={}", ms, dial),
					DeviceStateUpdate::EncoderUp(dial) => log::info!("[touch] t={} ENC_UP dial={}", ms, dial),
					DeviceStateUpdate::TouchPointDown(p) => log::info!("[touch] t={} TPOINT_DOWN {}", ms, p),
					DeviceStateUpdate::TouchPointUp(p) => log::info!("[touch] t={} TPOINT_UP {}", ms, p),
					_ => {}
				}
			}
			match match update {
				DeviceStateUpdate::ButtonDown(key) => inbound::devices::key_down(press(key)).await,
				DeviceStateUpdate::ButtonUp(key) => inbound::devices::key_up(press(key)).await,
				DeviceStateUpdate::TouchPointDown(point) => inbound::devices::key_down(press(kind.key_count() + point)).await,
				DeviceStateUpdate::TouchPointUp(point) => inbound::devices::key_up(press(kind.key_count() + point)).await,
				DeviceStateUpdate::EncoderTwist(dial, ticks) => inbound::devices::encoder_change(encoder(dial, ticks)).await,
				DeviceStateUpdate::EncoderDown(dial) => inbound::devices::encoder_down(press(dial)).await,
				DeviceStateUpdate::EncoderUp(dial) => inbound::devices::encoder_up(press(dial)).await,
				DeviceStateUpdate::TouchScreenPress(x, y) => inbound::devices::touch_tap(touch(x, y, false)).await,
				DeviceStateUpdate::TouchScreenLongPress(x, y) => inbound::devices::touch_tap(touch(x, y, true)).await,
				DeviceStateUpdate::TouchScreenSwipe(from, to) => inbound::devices::touch_swipe(device_id.clone(), from, to).await,
			} {
				Ok(_) => (),
				Err(error) => log::warn!("Failed to process device event {update:?}: {error}"),
			}
		}
	}

	ELGATO_DEVICES.write().await.remove(&device_id);
	LCD_FRAMEBUFFERS.write().await.remove(&device_id);
	crate::events::inbound::devices::deregister_device("", crate::events::inbound::PayloadEvent { payload: device_id })
		.await
		.unwrap();
}

/// Attempt to initialise all connected devices.
pub async fn initialise_devices() {
	if let Ok(settings) = crate::store::get_settings() {
		if settings.value.disableelgato {
			crate::plugins::DEVICE_NAMESPACES
				.write()
				.await
				.insert("sd".to_owned(), "opendeck_alternative_elgato_implementation".to_owned());
			return;
		} else {
			crate::plugins::DEVICE_NAMESPACES.write().await.remove("sd");
		}
	}

	// Iterate through detected Elgato devices and attempt to register them.
	let current = HIDAPI.read().await.as_ref().cloned();
	let hid = match current {
		Some(arc) => arc,
		None => match elgato_streamdeck::new_hidapi() {
			Ok(hid) => {
				let arc = Arc::new(hid);
				HIDAPI.write().await.replace(arc.clone());
				arc
			}
			Err(error) => {
				log::warn!("Failed to initialise hidapi: {error}");
				return;
			}
		},
	};
	for (kind, serial) in elgato_streamdeck::asynchronous::list_devices_async(&hid) {
		let device_id = format!("sd-{serial}");
		if ELGATO_DEVICES.read().await.contains_key(&device_id) {
			continue;
		}
		match elgato_streamdeck::AsyncStreamDeck::connect(&hid, kind, &serial) {
			Ok(device) => {
				tokio::spawn(init(device, device_id));
			}
			Err(error) => log::warn!("Failed to connect to Elgato device: {error}"),
		}
	}
}
