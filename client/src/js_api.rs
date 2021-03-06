use crate::canvas::*;
use crate::network::create_websocket_and_listen;
use instant::Instant;
use rust_us_core::*;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use wasm_bindgen::prelude::*;

// When the `wee_alloc` feature is enabled, use `wee_alloc` as the global
// allocator.
#[cfg(feature = "wee_alloc")]
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

// Exposes a JS API for a Canvas and a GameAsPlayer.
#[wasm_bindgen]
pub struct GameWrapper {
  canvas: Canvas,
  previous_frame_time: Instant,
  game: Arc<Mutex<Option<GameAsPlayer>>>,
  playback_server: Option<PlaybackServer>,
}

#[wasm_bindgen]
impl GameWrapper {
  #[allow(clippy::too_many_arguments)]
  pub fn set_inputs(
    &mut self,
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    kill: bool,
    report: bool,
    activate: bool,
    play: bool,
    skip_back: bool,
    skip_forward: bool,
    pause_playback: bool,
  ) -> Result<(), JsValue> {
    let mut game = self
      .game
      .lock()
      .expect("Internal Error: could not get a lock on the game");
    if game.is_none() {
      return Ok(());
    }
    let game = game.as_mut().unwrap();
    let prev_input = game.inputs();
    let input = InputState {
      up,
      down,
      left,
      right,
      kill,
      report,
      activate,
      play,
      skip_back,
      skip_forward,
      pause_playback,
    };
    if let Some(playback_server) = &mut self.playback_server {
      if input.skip_back && !prev_input.skip_back {
        let time = playback_server.current_time();
        playback_server
          .skip_to(
            time
              .checked_sub(Duration::from_secs(5))
              .unwrap_or_else(|| Duration::from_secs(0)),
            game,
          )
          .map_err(|e| JsValue::from(format!("{}", e)))?;
        console_log!("Skipped back");
      } else if input.skip_forward && !prev_input.skip_forward {
        let time = playback_server.current_time();
        playback_server
          .skip_to(
            time
              .checked_add(Duration::from_secs(5))
              .unwrap_or_else(|| playback_server.duration()),
            game,
          )
          .map_err(|e| JsValue::from(format!("{}", e)))?;
      } else if input.pause_playback && !prev_input.pause_playback {
        playback_server.toggle_pause();
        if !playback_server.paused() {
          self.previous_frame_time = Instant::now();
        }
      }
    }
    if game.state.status.finished() {
      return Ok(());
    }
    game.take_input(input).map_err(JsValue::from)
  }

  pub fn simulate(&mut self) -> Result<bool, JsValue> {
    let mut game = self
      .game
      .lock()
      .expect("Internal Error: could not get a lock on the game");
    if game.is_none() {
      return Ok(false);
    }
    let game = game.as_mut().unwrap();
    let now = Instant::now();
    let elapsed = now - self.previous_frame_time;
    self.previous_frame_time = now;
    if let Some(playback_server) = &mut self.playback_server {
      if playback_server.paused() {
        // Skip all simulation and drawing while paused until we
        // get the next input.
        return Ok(true);
      }
      playback_server
        .simulate(elapsed, game, false)
        .map_err(|e| JsValue::from(format!("{}", e)))?;
      self.write_time_offset_into_url();
    }
    if game.state.status == GameStatus::Connecting {
      return Ok(false);
    }
    Ok(game.simulate(elapsed))
  }

  fn write_time_offset_into_url(&self) {
    let playback_server = match &self.playback_server {
      None => return,
      Some(p) => p,
    };
    let window = web_sys::window().unwrap_throw();
    let href = window.location().href().unwrap_throw();
    let url = web_sys::Url::new(&href).unwrap_throw();
    url.set_search(&format!(
      "?recording&time={}",
      playback_server.current_time().as_secs()
    ));
    let new_href = url.href();
    if href != new_href {
      window
        .history()
        .unwrap_throw()
        .replace_state_with_url(&JsValue::null(), "Airlock.chat", Some(&new_href))
        .unwrap_throw();
    }
  }

  fn read_time_offset_from_url(&self) -> Option<Duration> {
    let window = web_sys::window().unwrap_throw();
    let href = window.location().href().unwrap_throw();
    let url = web_sys::Url::new(&href).unwrap_throw();
    let time = match url.search_params().get("time") {
      None => return None,
      Some(t) => t,
    };
    let time: f64 = time.parse().ok()?;
    Some(Duration::from_secs_f64(time))
  }

  pub fn draw(&mut self) -> Result<(), JsValue> {
    self.canvas.draw(self.game.clone())
  }
}

fn get_recorded_game() -> Result<Option<RecordedGame>, JsValue> {
  let local_storage = web_sys::window()
    .ok_or("no window")?
    .local_storage()?
    .ok_or("no window.localStorage")?;
  let value = local_storage.get("latest game")?;
  let encoded_game = match value {
    None => return Ok(None),
    Some(g) => g,
  };
  let game = serde_json::from_str(&encoded_game).map_err(|e| {
    format!(
      "Unable to decode game recording from localStorage {:?} – {:?}",
      encoded_game, e
    )
  })?;
  let game = match game {
    ServerToClientMessage::Replay(game) => game,
    _ => {
      return Err(
        format!(
          "Could not decode recorded game from local storage. Expected a Replay but found a {}",
          game.kind()
        )
        .into(),
      )
    }
  };
  Ok(Some(game))
}

pub fn save_recorded_game(encoded_game: &str) -> Result<(), JsValue> {
  let local_storage = web_sys::window()
    .ok_or("no window")?
    .local_storage()?
    .ok_or("no window.localStorage")?;
  local_storage.set("latest game", encoded_game)?;
  Ok(())
}

#[wasm_bindgen]
pub fn make_game(name: String) -> Result<GameWrapper, JsValue> {
  crate::utils::set_panic_hook();
  let location = web_sys::window().ok_or("no window")?.location();
  let should_playback = location.search()?.contains("recording");
  let spectate = location.search()?.contains("spectate");
  let mut wrapper;
  if !should_playback {
    wrapper = GameWrapper {
      previous_frame_time: Instant::now(),
      canvas: Canvas::find_in_document()?,
      game: Arc::new(Mutex::new(None)),
      playback_server: None,
    };
    let join = if spectate {
      JoinRequest::JoinAsSpectator
    } else {
      JoinRequest::JoinAsPlayer {
        name,
        preferred_color: Color::random(),
      }
    };
    create_websocket_and_listen(wrapper.game.clone(), join)?;
  } else {
    let recording = match get_recorded_game()? {
      None => return Err(JsValue::from("No saved game found")),
      Some(recording) => recording,
    };
    console_log!(
      "Starting replay of version {} inside game with version {}",
      recording.version,
      get_version_sha()
    );
    let connection = Box::new(PlaybackTx {});
    let mut game_as_player = GameAsPlayer::new(UUID::random(), connection);
    game_as_player.state.status = GameStatus::Lobby;
    wrapper = GameWrapper {
      previous_frame_time: Instant::now(),
      canvas: Canvas::find_in_document()?,
      playback_server: Some(PlaybackServer::new(recording)),
      game: Arc::new(Mutex::new(Some(game_as_player))),
    };
    if let Some(offset) = wrapper.read_time_offset_from_url() {
      if let Some(playback_server) = &mut wrapper.playback_server {
        let mut game = wrapper.game.lock().unwrap_throw();
        let game = game.as_mut().unwrap_throw();
        playback_server.skip_to(offset, game).unwrap_throw();
        playback_server.toggle_pause();
      }
    }
  }

  Ok(wrapper)
}
