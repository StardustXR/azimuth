use color::rgba;
use color_eyre::Result;
use glam::Quat;
use input_event_codes::{BTN_LEFT, BTN_RIGHT};
use mint::Vector2;
use serde::{Deserialize, Serialize};
use stardust_xr_fusion::{
	client::{Client, FrameInfo, RootHandler},
	core::{schemas::flex::flexbuffers, values::Transform},
	drawable::Lines,
	fields::{Field, RayMarchResult, SphereField},
	input::{InputHandler, PointerInputMethod},
	node::NodeType,
};
use stardust_xr_molecules::{
	data::InlinePulseReceiver,
	lines::{circle, make_line_points},
	mouse::{MouseEvent as MouseReceiverEvent, MOUSE_MASK},
};
use tokio::{sync::mpsc::Receiver, task::JoinSet};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
	color_eyre::install().unwrap();
	let (client, event_loop) = Client::connect_with_async_loop()
		.await
		.expect("Couldn't connect");

	let (mouse_event_tx, mouse_event_rx) = tokio::sync::mpsc::channel(64);
	let azimuth = client.wrap_root(Azimuth::create(&client, mouse_event_rx)?)?;
	let field = SphereField::create(&azimuth.lock().pointer, [0.0; 3], 0.0)?;
	let _mouse_pulse_receiver = InlinePulseReceiver::create(
		&azimuth.lock().pointer,
		Transform::default(),
		&field,
		&MOUSE_MASK,
		move |_uid, raw, _reader| {
			let Some(mouse_event) = MouseReceiverEvent::from_pulse_data(raw) else {return};
			if let Some(mouse_delta) = mouse_event.delta {
				let _ = mouse_event_tx.try_send(MouseEvent::Moved {
					x: mouse_delta.x,
					y: mouse_delta.y,
				});
			}
			if let Some(buttons_down) = mouse_event.buttons_down {
				for button in buttons_down {
					if button == BTN_LEFT!() {
						let _ = mouse_event_tx.try_send(MouseEvent::LeftClick(true));
					}
					if button == BTN_RIGHT!() {
						let _ = mouse_event_tx.try_send(MouseEvent::RightClick(true));
					}
				}
			}
			if let Some(buttons_up) = mouse_event.buttons_up {
				for button in buttons_up {
					if button == BTN_LEFT!() {
						let _ = mouse_event_tx.try_send(MouseEvent::LeftClick(false));
					}
					if button == BTN_RIGHT!() {
						let _ = mouse_event_tx.try_send(MouseEvent::RightClick(false));
					}
				}
			}
			if let Some(scroll_distance) = mouse_event.scroll_distance {
				let _ = mouse_event_tx.try_send(MouseEvent::Scroll {
					x: scroll_distance.x,
					y: scroll_distance.y,
				});
			}
			if let Some(scroll_steps) = mouse_event.scroll_steps {
				let _ = mouse_event_tx.try_send(MouseEvent::ScrollDiscrete {
					x: scroll_steps.x,
					y: scroll_steps.y,
				});
			}
		},
	)?;

	tokio::select! {
		biased;
		_ = tokio::signal::ctrl_c() => Ok(()),
		e = event_loop => e?.map_err(|e| e.into()),
	}
}

enum MouseEvent {
	Moved { x: f32, y: f32 },
	LeftClick(bool),
	RightClick(bool),
	Scroll { x: f32, y: f32 },
	ScrollDiscrete { x: f32, y: f32 },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Datamap {
	select: f32,
	grab: f32,
	scroll: Vector2<f32>,
}
impl Datamap {
	pub fn serialize_pulse_data(&self) -> Vec<u8> {
		let mut serializer = flexbuffers::FlexbufferSerializer::new();
		let _ = self.serialize(&mut serializer);
		serializer.take_buffer()
	}
}

// degrees per pixel, constant for now since i'm lazy
const MOUSE_SENSITIVITY: f32 = 0.1;
struct Azimuth {
	pointer: PointerInputMethod,
	mouse_event_rx: Receiver<MouseEvent>,
	_lines: Lines,
	yaw: f32,
	pitch: f32,
	datamap: Datamap,
}
impl Azimuth {
	pub fn create(client: &Client, mouse_event_rx: Receiver<MouseEvent>) -> Result<Self> {
		let pointer = PointerInputMethod::create(client.get_root(), Transform::identity(), None)?;
		let line_points =
			make_line_points(&circle(8, 0.0, 0.0005), 0.001, rgba!(1.0, 1.0, 1.0, 1.0));
		let lines = Lines::create(
			&pointer,
			Transform::from_position([0.0, 0.0, -0.1]),
			&line_points,
			true,
		)?;

		Ok(Azimuth {
			pointer,
			mouse_event_rx,
			_lines: lines,
			yaw: 0.0,
			pitch: 0.0,
			datamap: Datamap {
				select: 0.0,
				grab: 0.0,
				scroll: [0.0; 2].into(),
			},
		})
	}
}
impl RootHandler for Azimuth {
	fn frame(&mut self, _info: FrameInfo) {
		let Ok(client) = self.pointer.client() else {return};
		let _ = self.pointer.set_position(Some(client.get_hmd()), [0.0; 3]);

		self.datamap.scroll = [0.0; 2].into();
		while let Ok(mouse_event) = self.mouse_event_rx.try_recv() {
			match mouse_event {
				MouseEvent::Moved { x, y } => {
					self.yaw += x * MOUSE_SENSITIVITY;
					self.pitch += y * MOUSE_SENSITIVITY;
					self.pitch = self.pitch.clamp(-90.0, 90.0);

					let rotation_x = Quat::from_rotation_x(-self.pitch.to_radians());
					let rotation_y = Quat::from_rotation_y(-self.yaw.to_radians());
					let _ = self.pointer.set_rotation(None, rotation_y * rotation_x);
				}
				MouseEvent::LeftClick(c) => self.datamap.select = if c { 1.0 } else { 0.0 },
				MouseEvent::RightClick(c) => self.datamap.grab = if c { 1.0 } else { 0.0 },
				MouseEvent::Scroll { x, y } => self.datamap.scroll = [x, y].into(),
				MouseEvent::ScrollDiscrete { x, y } => self.datamap.scroll = [x, y].into(),
			}
		}
		let _ = self
			.pointer
			.set_datamap(self.datamap.serialize_pulse_data().as_slice());

		let pointer = self.pointer.alias();
		tokio::task::spawn(async move {
			let mut closest_hits: Option<(Vec<InputHandler>, RayMarchResult)> = None;
			let mut join = JoinSet::new();
			for handler in pointer.alias().input_handlers().values() {
				let Some(field) = handler.field() else {continue};
				let Ok(ray_march_result) = field.ray_march(&pointer, [0.0; 3], [0.0, 0.0, -1.0]) else {continue};
				let handler = handler.alias();
				join.spawn(async move { (handler, ray_march_result.await) });
			}

			while let Some(res) = join.join_next().await {
				let Ok((handler, Ok(ray_info))) = res else {continue};
				if !ray_info.hit() {
					continue;
				}
				if let Some((hit_handlers, hit_info)) = &mut closest_hits {
					if ray_info.deepest_point_distance == hit_info.deepest_point_distance {
						hit_handlers.push(handler);
					} else if ray_info.deepest_point_distance < hit_info.deepest_point_distance {
						*hit_handlers = vec![handler];
						*hit_info = ray_info;
					}
				} else {
					closest_hits.replace((vec![handler], ray_info));
				}
			}

			if let Some((hit_handlers, _hit_info)) = closest_hits {
				let _ =
					pointer.set_handler_order(hit_handlers.iter().collect::<Vec<_>>().as_slice());
			} else {
				let _ = pointer.set_handler_order(&[]);
			}
		});
	}
}
