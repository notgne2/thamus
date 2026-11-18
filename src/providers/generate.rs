use anyhow::{Context as _, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    pin,
};
use tracing::{instrument, trace};

fn calc_from_loose_res(loose_res: (u32, u32), megapixels: f64) -> (u32, u32) {
    // turn an intentionally loosely confurable res into a real one.
    // Both (1024,1024) and (5,5) should become (1024,1024) with megapixels of 1.0
    // (512, 1024) should become (1024, 2048) with megapixels of 2.0
    let aspect_ratio = f64::from(loose_res.0) / f64::from(loose_res.1);
    let total_pixels = megapixels * 1_000_000.0;
    let w = (total_pixels * aspect_ratio).sqrt().round() as u32;
    let h = (total_pixels / aspect_ratio).sqrt().round() as u32;

    (w, h)
}

#[instrument(level = "info", skip_all)]
pub async fn generate_song(
    client: &reqwest::Client,
    audio_cpp_uri: &str,

    tags: &str,
    lyrics: &str,
    duration: u16,

    save_to: impl AsyncWrite,
) -> anyhow::Result<()> {
    let res = client
        .post(format!("{audio_cpp_uri}/v1/tasks/run"))
        .json(&serde_json::json!({
            "model": "ace-step-1.5-turbo",
            "request": {
                "text": tags,
                "lyrics": lyrics,
                "duration_seconds": duration,
                // "options": {"tags": "pop,bright"}
            }
        }))
        .send()
        .await
        .context("Sending music generation request")?;

    let json_response = res.json::<serde_json::Value>().await?;

    let audio_base64 = &json_response["audio"]
        .as_str()
        .with_context(|| format!("audio field missing or not a string: {json_response:#?}"))?;
    let audio_bytes = STANDARD.decode(audio_base64)?;

    if audio_bytes.len() < 500 {
        match std::str::from_utf8(&audio_bytes) {
            Ok(s) => bail!("Audio appears to be a short string? Assuming it's an error:\n{s}"),
            Err(_e) => bail!("Audio is unusually small, unknown error"),
        }
    }

    pin!(save_to);

    save_to
        .write_all(&audio_bytes)
        .await
        .context("Saving audio file")?;

    save_to.flush().await.context("Flushing audio file")?;

    Ok(())
}

#[instrument(level = "info", skip_all)]
pub async fn generate_image(
    client: &reqwest::Client,
    sd_cpp_uri: &str,

    in_files: &mut [impl AsyncRead + Unpin],

    prompt: &str,
    mut loose_res: Option<(u32, u32)>,

    save_to: impl AsyncWrite,
) -> anyhow::Result<()> {
    let mut in_blobs = Vec::new();

    for in_file in in_files {
        let mut in_blob = Vec::new();
        in_file
            .read_to_end(&mut in_blob)
            .await
            .context("Reading input image")?;
        in_blobs.push(in_blob);
    }

    let mut in_base64s = Vec::new();

    for in_blob in &in_blobs {
        let in_base64 = STANDARD.encode(in_blob);
        in_base64s.push(in_base64);
    }

    if let (None, Some(first_in)) = (loose_res, in_blobs.first()) {
        let size = imagesize::blob_size(first_in).context("Probing first input image size")?;
        loose_res = Some((size.width as u32, size.height as u32));
    }

    let mut req = json!({
        "prompt": prompt,
        "extra_images": in_base64s,
        "auto_resize_ref_image": true,
    });

    if let Some(loose_res) = loose_res {
        let res = calc_from_loose_res(loose_res, 1.0);

        req["width"] = json!(res.0);
        req["height"] = json!(res.1);
    }

    let res = client
        .post(format!("{sd_cpp_uri}/sdapi/v1/txt2img"))
        .json(&req)
        .send()
        .await
        .context("Sending image generation request")?;

    let json_response = res.json::<serde_json::Value>().await?;

    if let Some(error) = json_response.get("error").and_then(|v| v.as_str()) {
        bail!("Generation error: {error}");
    }

    let [image_value] = json_response["images"]
        .as_array()
        .context("images missing from result")?
        .as_slice()
    else {
        bail!("did not return exactly 1 image");
    };

    let image_base64 = image_value.as_str().context("image entry not a string")?;

    let image_bytes = STANDARD
        .decode(image_base64)
        .context("Failed to decode image base64")?;

    pin!(save_to);

    save_to
        .write_all(&image_bytes)
        .await
        .context("Saving image file")?;

    save_to.flush().await.context("Flushing image file")?;

    Ok(())
}

#[instrument(level = "info", skip_all)]
pub async fn generate_speech(
    client: &reqwest::Client,
    audio_cpp_uri: &str,

    model: &str,

    reference_audio_path: &str,
    reference_text: &str,

    words: &str,

    save_to: impl AsyncWrite,
) -> anyhow::Result<()> {
    let res = client
        .post(format!("{audio_cpp_uri}/v1/audio/speech"))
        .json(&serde_json::json!({
            "model": model,
            "input": words,
            "seed": 999,
            "language": "en",
            "voice_ref": format!("samples/{reference_audio_path}"),
            "reference_text": reference_text,
        }))
        .send()
        .await
        .context("Sending speech generation request")?;

    let song_response = res.bytes().await?;

    pin!(save_to);

    save_to
        .write_all(&song_response)
        .await
        .context("Saving audio file")?;

    save_to.flush().await.context("Flushing audio file")?;

    Ok(())
}

#[instrument(level = "info", skip_all)]
pub async fn decode_speech(
    client: &reqwest::Client,
    audio_cpp_uri: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<String> {
    let file_part = reqwest::multipart::Part::bytes(bytes);

    let multipart_form = reqwest::multipart::Form::new()
        .text("model", "qwen3-asr")
        .text("language", "en")
        .part("file", file_part);

    let res = client
        .post(format!("{audio_cpp_uri}/v1/audio/transcriptions"))
        .multipart(multipart_form)
        .send()
        .await
        .context("Sending speech transcription request")?;

    let json_response = res.json::<serde_json::Value>().await?;

    trace!("json_response: {:#?}", json_response);

    if let Some(error) = json_response.get("error").and_then(|v| v.as_str()) {
        bail!("Transcription error: {error}");
    }

    let text = json_response
        .get("text")
        .and_then(|v| v.as_str())
        .context("Invalid or missing text field")?;

    Ok(text.to_string())
}
