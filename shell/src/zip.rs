//! Deflate, from the browser rather than from a crate.
//!
//! An SDP is a few hundred bytes of extremely repetitive ASCII — the same
//! attribute names over and over — so it compresses to roughly a third. That
//! matters because the whole handshake travels through a human: a shorter link
//! is a sparser QR code, and a QR code that a phone can actually read.
//!
//! `CompressionStream` is a platform feature, so this costs no dependency and
//! no wasm bytes. It is stream-shaped, hence the Blob/Response dance: a Blob
//! gives us a stream to pipe, and a Response collects one back into bytes.
//!
//! web-sys hides both constructors behind `--cfg=web_sys_unstable_apis`, a
//! global flag every build and CI job would then have to set. Declaring the
//! two of them here costs a dozen lines and keeps the build ordinary. Both
//! shipped in every current browser; the callers fall back to a plain link if
//! the constructor throws.

use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, ReadableStream, ReadableWritablePair, Response, WritableStream};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = CompressionStream)]
    type RawDeflate;
    #[wasm_bindgen(constructor, js_class = "CompressionStream", catch)]
    fn new(format: &str) -> Result<RawDeflate, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn readable(this: &RawDeflate) -> ReadableStream;
    #[wasm_bindgen(method, getter)]
    fn writable(this: &RawDeflate) -> WritableStream;

    #[wasm_bindgen(js_name = DecompressionStream)]
    type RawInflate;
    #[wasm_bindgen(constructor, js_class = "DecompressionStream", catch)]
    fn new(format: &str) -> Result<RawInflate, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn readable(this: &RawInflate) -> ReadableStream;
    #[wasm_bindgen(method, getter)]
    fn writable(this: &RawInflate) -> WritableStream;
}

/// `deflate-raw` rather than `deflate`: no zlib header or trailer, six bytes
/// we would otherwise base64 for nothing.
const FORMAT: &str = "deflate-raw";

fn as_stream(bytes: &[u8]) -> Result<ReadableStream, JsValue> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    Ok(Blob::new_with_u8_array_sequence(&parts)?.stream())
}

async fn collect(stream: &ReadableStream) -> Result<Vec<u8>, JsValue> {
    let resp = Response::new_with_opt_readable_stream(Some(stream))?;
    let buf = JsFuture::from(resp.array_buffer()?).await?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

pub async fn deflate(bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    let cs = RawDeflate::new(FORMAT)?;
    let pair = ReadableWritablePair::new(&cs.readable(), &cs.writable());
    collect(&as_stream(bytes)?.pipe_through(&pair)).await
}

pub async fn inflate(bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    let ds = RawInflate::new(FORMAT)?;
    let pair = ReadableWritablePair::new(&ds.readable(), &ds.writable());
    collect(&as_stream(bytes)?.pipe_through(&pair)).await
}
