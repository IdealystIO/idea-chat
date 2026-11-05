//! Getting a file from the person to the bucket: pick it, reserve a slot,
//! put the bytes straight into storage, confirm.

use chat_api::files::Purpose;
use chat_model::Attachment;
use file_picker::{FilePicker, PickOutcome, PickRequest, PickedFile};

/// Ask for files. `images` limits the picker to pictures.
pub async fn pick(images: bool, many: bool) -> Vec<PickedFile> {
    let request = if images {
        PickRequest::media(file_picker::MediaKind::Images)
    } else {
        PickRequest::documents(Vec::<String>::new())
    };
    let request = if many { request.multiple() } else { request };
    match FilePicker::new().pick(request).await {
        Ok(PickOutcome::Picked(files)) => files,
        _ => Vec::new(),
    }
}

/// Upload one picked file. Returns the attachment, or why it failed.
pub async fn upload(file: &PickedFile, purpose: Purpose) -> Result<Attachment, String> {
    let bytes = file.read_all().await.map_err(|e| format!("Couldn't read {}: {e:?}", file.name()))?;
    let mime = if file.mime().is_empty() { "application/octet-stream".to_string() } else { file.mime().to_string() };
    let slot = chat_api::files::begin_upload(file.name().to_string(), bytes.len() as u64, mime, purpose)
        .await
        .map_err(|e| chat_api::describe(&e))?;
    let dims = imagesize::blob_size(&bytes).ok().map(|d| (d.width as u32, d.height as u32));
    let put = net::Client::new()
        .put(&slot.put_url)
        .header("content-type", slot.content_type.clone())
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("Upload failed: {e}"))?;
    if !put.is_success() {
        return Err(format!("Upload failed (storage said {}).", put.status()));
    }
    chat_api::files::finish_upload(slot.id, dims.map(|d| d.0), dims.map(|d| d.1)).await.map_err(|e| chat_api::describe(&e))
}
