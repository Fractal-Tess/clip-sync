use tokio_util::codec::LengthDelimitedCodec;

pub const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;

#[must_use]
pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_IPC_FRAME_BYTES)
        .new_codec()
}
