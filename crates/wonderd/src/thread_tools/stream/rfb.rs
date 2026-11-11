//! A minimal, view-only RFB (VNC) client for the remote-screen tool.
//!
//! It sends only SetPixelFormat, SetEncodings (Raw) and FramebufferUpdateRequest: there is no pointer, key or
//! clipboard message in this file, so a remote screen can never be driven from here. It accepts only security
//! type None: the channel is an already authenticated SSH session, and Cupola holds no VNC passwords.
//! Everything the server says is bounded: framebuffer size, rectangles, skipped text and names.
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 8.5 megapixels: 3840x2160 fits. At four bytes a pixel that is under 34 MB per stream.
pub(crate) const MAX_PIXELS: u64 = 8_500_000;
const MAX_NAME: usize = 4096;
const MAX_SKIPPED: usize = 1 << 20;
const MAX_REASON: usize = 4096;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum RfbError {
    /// Not RFB, or a message out of order or out of bounds.
    Protocol,
    /// The server needs a password or another security type than None.
    AuthRequired,
    /// The server refused the connection.
    Refused,
    /// The framebuffer is larger than `MAX_PIXELS`.
    TooLarge,
    /// The server sent an encoding that was not asked for.
    Unsupported,
    /// The connection closed or failed.
    Io,
}

impl From<std::io::Error> for RfbError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// The remote screen as 32-bit pixels, little-endian BGRX (blue first): what the fixed pixel format asks for.
#[derive(Debug)]
pub(crate) struct Framebuffer {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bgrx: Vec<u8>,
}

impl Framebuffer {
    fn new(width: u32, height: u32) -> Result<Self, RfbError> {
        if width == 0 || height == 0 {
            return Err(RfbError::Protocol);
        }
        if u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(RfbError::TooLarge);
        }
        Ok(Self {
            width,
            height,
            bgrx: vec![0; width as usize * height as usize * 4],
        })
    }
}

async fn u8_of<R: AsyncRead + Unpin>(io: &mut R) -> Result<u8, RfbError> {
    Ok(io.read_u8().await?)
}

async fn skip<R: AsyncRead + Unpin>(io: &mut R, mut count: usize) -> Result<(), RfbError> {
    let mut buffer = [0u8; 4096];
    while count > 0 {
        let n = count.min(buffer.len());
        io.read_exact(&mut buffer[..n]).await?;
        count -= n;
    }
    Ok(())
}

/// A length-prefixed text, read and dropped within `max`.
async fn skip_text<R: AsyncRead + Unpin>(io: &mut R, max: usize) -> Result<(), RfbError> {
    let length = io.read_u32().await? as usize;
    if length > max {
        return Err(RfbError::Protocol);
    }
    skip(io, length).await
}

/// RFB handshake through ClientInit/ServerInit, then the fixed pixel format and Raw encoding.
/// Returns the screen, still black until the first update is read.
pub(crate) async fn connect<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
) -> Result<Framebuffer, RfbError> {
    let mut version = [0u8; 12];
    io.read_exact(&mut version).await?;
    if &version[..8] != b"RFB 003."
        || version[11] != b'\n'
        || !version[8..11].iter().all(u8::is_ascii_digit)
    {
        return Err(RfbError::Protocol);
    }
    let minor: u32 = std::str::from_utf8(&version[8..11])
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(RfbError::Protocol)?;
    // 3.8 and 3.7 are spoken as themselves; anything older (or odd) as 3.3.
    let chosen: &[u8; 12] = if minor >= 8 {
        b"RFB 003.008\n"
    } else if minor == 7 {
        b"RFB 003.007\n"
    } else {
        b"RFB 003.003\n"
    };
    io.write_all(chosen).await?;
    io.flush().await?;
    if minor >= 7 {
        let count = u8_of(io).await?;
        if count == 0 {
            // The server says why it refused; the words are dropped.
            let _ = skip_text(io, MAX_REASON).await;
            return Err(RfbError::Refused);
        }
        let mut types = vec![0u8; count as usize];
        io.read_exact(&mut types).await?;
        if !types.contains(&1) {
            return Err(RfbError::AuthRequired);
        }
        io.write_all(&[1]).await?;
        io.flush().await?;
        // 3.7 sends no result for None; 3.8 does.
        if minor >= 8 {
            match io.read_u32().await? {
                0 => {}
                _ => {
                    let _ = skip_text(io, MAX_REASON).await;
                    return Err(RfbError::Refused);
                }
            }
        }
    } else {
        match io.read_u32().await? {
            1 => {}
            0 => {
                let _ = skip_text(io, MAX_REASON).await;
                return Err(RfbError::Refused);
            }
            _ => return Err(RfbError::AuthRequired),
        }
    }
    // ClientInit: shared, so a person at the screen is not disconnected.
    io.write_all(&[1]).await?;
    io.flush().await?;
    let width = u32::from(io.read_u16().await?);
    let height = u32::from(io.read_u16().await?);
    skip(io, 16).await?; // The server's pixel format: replaced below.
    skip_text(io, MAX_NAME).await?; // Desktop name.
    let framebuffer = Framebuffer::new(width, height)?;
    // SetPixelFormat: 32 bits, depth 24, little-endian, true colour, 8 bits a channel, R<<16 G<<8 B.
    let mut set_format = vec![0u8, 0, 0, 0, 32, 24, 0, 1];
    set_format.extend_from_slice(&255u16.to_be_bytes());
    set_format.extend_from_slice(&255u16.to_be_bytes());
    set_format.extend_from_slice(&255u16.to_be_bytes());
    set_format.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
    io.write_all(&set_format).await?;
    // SetEncodings: Raw only.
    io.write_all(&[2, 0, 0, 1, 0, 0, 0, 0]).await?;
    io.flush().await?;
    Ok(framebuffer)
}

/// FramebufferUpdateRequest for the whole screen.
pub(crate) async fn request<W: AsyncWrite + Unpin>(
    io: &mut W,
    framebuffer: &Framebuffer,
    incremental: bool,
) -> Result<(), RfbError> {
    let mut message = vec![3u8, u8::from(incremental), 0, 0, 0, 0];
    message.extend_from_slice(&(framebuffer.width as u16).to_be_bytes());
    message.extend_from_slice(&(framebuffer.height as u16).to_be_bytes());
    io.write_all(&message).await?;
    io.flush().await?;
    Ok(())
}

/// Reads server messages until one FramebufferUpdate has been applied. Returns the number of rectangles.
pub(crate) async fn read_update<R: AsyncRead + Unpin>(
    io: &mut R,
    framebuffer: &mut Framebuffer,
) -> Result<usize, RfbError> {
    loop {
        match u8_of(io).await? {
            0 => {
                skip(io, 1).await?;
                let rectangles = io.read_u16().await? as usize;
                for _ in 0..rectangles {
                    let x = u32::from(io.read_u16().await?);
                    let y = u32::from(io.read_u16().await?);
                    let width = u32::from(io.read_u16().await?);
                    let height = u32::from(io.read_u16().await?);
                    let encoding = io.read_i32().await?;
                    if encoding != 0 {
                        return Err(RfbError::Unsupported);
                    }
                    if x.checked_add(width)
                        .is_none_or(|end| end > framebuffer.width)
                        || y.checked_add(height)
                            .is_none_or(|end| end > framebuffer.height)
                    {
                        return Err(RfbError::Protocol);
                    }
                    let stride = framebuffer.width as usize * 4;
                    let row_bytes = width as usize * 4;
                    for row in 0..height as usize {
                        let start = (y as usize + row) * stride + x as usize * 4;
                        io.read_exact(&mut framebuffer.bgrx[start..start + row_bytes])
                            .await?;
                    }
                }
                return Ok(rectangles);
            }
            // SetColourMapEntries: not used with true colour, skipped within bounds.
            1 => {
                skip(io, 1).await?;
                io.read_u16().await?;
                let entries = io.read_u16().await? as usize;
                skip(io, entries * 6).await?;
            }
            // Bell.
            2 => {}
            // ServerCutText: the remote clipboard never reaches Cupola.
            3 => {
                skip(io, 3).await?;
                skip_text(io, MAX_SKIPPED).await?;
            }
            _ => return Err(RfbError::Protocol),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream};

    /// A fake server that speaks `version`, offers `types`, and then `screen` as ServerInit.
    pub(crate) async fn serve_handshake(
        server: &mut DuplexStream,
        version: &[u8; 12],
        types: &[u8],
        width: u16,
        height: u16,
    ) {
        server.write_all(version).await.unwrap();
        let mut reply = [0u8; 12];
        server.read_exact(&mut reply).await.unwrap();
        let minor: u32 = std::str::from_utf8(&version[8..11])
            .unwrap()
            .parse()
            .unwrap();
        if minor >= 7 {
            let mut offer = vec![types.len() as u8];
            offer.extend_from_slice(types);
            server.write_all(&offer).await.unwrap();
            if types.is_empty() {
                let reason = b"go away";
                server
                    .write_all(&(reason.len() as u32).to_be_bytes())
                    .await
                    .unwrap();
                server.write_all(reason).await.unwrap();
                return;
            }
            let mut choice = [0u8; 1];
            server.read_exact(&mut choice).await.unwrap();
            if minor >= 8 {
                server.write_all(&0u32.to_be_bytes()).await.unwrap();
            }
        } else {
            server.write_all(&1u32.to_be_bytes()).await.unwrap();
        }
        let mut shared = [0u8; 1];
        server.read_exact(&mut shared).await.unwrap();
        server.write_all(&width.to_be_bytes()).await.unwrap();
        server.write_all(&height.to_be_bytes()).await.unwrap();
        server.write_all(&[0u8; 16]).await.unwrap();
        server.write_all(&4u32.to_be_bytes()).await.unwrap();
        server.write_all(b"test").await.unwrap();
        // SetPixelFormat (20) + SetEncodings (8).
        let mut formats = [0u8; 28];
        server.read_exact(&mut formats).await.unwrap();
        assert_eq!(formats[0], 0);
        assert_eq!(&formats[4..8], &[32, 24, 0, 1]);
        assert_eq!(&formats[20..28], &[2, 0, 0, 1, 0, 0, 0, 0]);
    }

    /// x, y, width, height, encoding and pixel bytes of one rectangle.
    type Rect = (u16, u16, u16, u16, i32, Vec<u8>);

    fn update(rects: &[Rect]) -> Vec<u8> {
        let mut bytes = vec![0u8, 0];
        bytes.extend_from_slice(&(rects.len() as u16).to_be_bytes());
        for (x, y, w, h, enc, data) in rects {
            for v in [x, y, w, h] {
                bytes.extend_from_slice(&v.to_be_bytes());
            }
            bytes.extend_from_slice(&enc.to_be_bytes());
            bytes.extend_from_slice(data);
        }
        bytes
    }

    #[tokio::test]
    async fn speaks_all_three_versions_and_applies_a_raw_update() {
        for version in [
            b"RFB 003.008\n",
            b"RFB 003.007\n",
            b"RFB 003.003\n",
            b"RFB 003.889\n",
        ] {
            let (mut client, mut server) = duplex(1 << 16);
            let server_task = tokio::spawn(async move {
                serve_handshake(&mut server, version, &[2, 1], 4, 2).await;
                let mut request = [0u8; 10];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(request[0], 3);
                server
                    .write_all(&update(&[(1, 0, 2, 2, 0, vec![9; 16])]))
                    .await
                    .unwrap();
                server
            });
            let mut fb = connect(&mut client).await.unwrap();
            assert_eq!((fb.width, fb.height), (4, 2));
            request(&mut client, &fb, false).await.unwrap();
            assert_eq!(read_update(&mut client, &mut fb).await, Ok(1));
            // Only the 2x2 rectangle at x=1 changed.
            assert_eq!(&fb.bgrx[0..4], &[0; 4]);
            assert_eq!(&fb.bgrx[4..12], &[9; 8]);
            assert_eq!(&fb.bgrx[12..16], &[0; 4]);
            assert_eq!(&fb.bgrx[16 + 4..16 + 12], &[9; 8]);
            drop(server_task.await.unwrap());
        }
    }

    #[tokio::test]
    async fn refuses_passwords_failures_and_non_rfb_without_sending_more() {
        // A password-only server.
        let (mut client, mut server) = duplex(4096);
        tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut reply = [0u8; 12];
            server.read_exact(&mut reply).await.unwrap();
            server.write_all(&[1, 2]).await.unwrap();
            // The client must not answer a type it did not choose.
            let mut extra = [0u8; 1];
            assert_eq!(server.read(&mut extra).await.unwrap(), 0);
        });
        assert_eq!(
            connect(&mut client).await.err(),
            Some(RfbError::AuthRequired)
        );
        drop(client);

        // A refusing server with a reason.
        let (mut client, mut server) = duplex(4096);
        tokio::spawn(async move {
            serve_handshake(&mut server, b"RFB 003.008\n", &[], 1, 1).await;
        });
        assert_eq!(connect(&mut client).await.err(), Some(RfbError::Refused));

        // Not RFB at all, and a banner-shaped lie.
        for banner in [
            &b"SSH-2.0-OpenS"[..],
            b"RFB 004.000\n",
            b"RFB 003.0x8\n",
            b"\x00\x00",
        ] {
            let (mut client, mut server) = duplex(4096);
            let sent = banner.to_vec();
            tokio::spawn(async move {
                let _ = server.write_all(&sent).await;
                // Dropping closes the connection, as a server that hung up would.
            });
            let result = connect(&mut client).await;
            assert!(
                matches!(result, Err(RfbError::Protocol) | Err(RfbError::Io)),
                "{banner:?}"
            );
        }
    }

    #[tokio::test]
    async fn bounds_the_framebuffer_before_allocating_it() {
        for (w, h, expected) in [
            (0u16, 10u16, RfbError::Protocol),
            (10, 0, RfbError::Protocol),
            (65535, 65535, RfbError::TooLarge),
            (4000, 2200, RfbError::TooLarge),
        ] {
            let (mut client, mut server) = duplex(4096);
            tokio::spawn(async move {
                // The client stops after ServerInit, so the fake server stops writing there too.
                server.write_all(b"RFB 003.008\n").await.unwrap();
                let mut reply = [0u8; 12];
                server.read_exact(&mut reply).await.unwrap();
                server.write_all(&[1, 1]).await.unwrap();
                let mut choice = [0u8; 1];
                server.read_exact(&mut choice).await.unwrap();
                server.write_all(&0u32.to_be_bytes()).await.unwrap();
                server.read_exact(&mut choice).await.unwrap();
                server.write_all(&w.to_be_bytes()).await.unwrap();
                server.write_all(&h.to_be_bytes()).await.unwrap();
                server.write_all(&[0u8; 16]).await.unwrap();
                server.write_all(&0u32.to_be_bytes()).await.unwrap();
            });
            assert_eq!(connect(&mut client).await.err(), Some(expected), "{w}x{h}");
        }
        // 3840x2160 is inside the bound.
        assert!(Framebuffer::new(3840, 2160).is_ok());
    }

    #[tokio::test]
    async fn a_hostile_server_cannot_write_outside_the_screen_or_flood() {
        let cases: Vec<(&str, Vec<u8>, RfbError)> = vec![
            (
                "rectangle past the right edge",
                update(&[(3, 0, 2, 1, 0, vec![0; 8])]),
                RfbError::Protocol,
            ),
            (
                "rectangle past the bottom",
                update(&[(0, 1, 1, 2, 0, vec![0; 8])]),
                RfbError::Protocol,
            ),
            (
                "overflowing coordinates",
                update(&[(65535, 65535, 65535, 65535, 0, vec![])]),
                RfbError::Protocol,
            ),
            (
                "an encoding that was not asked for",
                update(&[(0, 0, 1, 1, 7, vec![0; 4])]),
                RfbError::Unsupported,
            ),
            ("an unknown message", vec![99], RfbError::Protocol),
            (
                "huge cut text",
                {
                    let mut bytes = vec![3, 0, 0, 0];
                    bytes.extend_from_slice(&(2u32 << 20).to_be_bytes());
                    bytes
                },
                RfbError::Protocol,
            ),
            (
                "truncated pixels",
                update(&[(0, 0, 2, 2, 0, vec![0; 3])]),
                RfbError::Io,
            ),
        ];
        for (name, bytes, expected) in cases {
            let (mut client, mut server) = duplex(1 << 16);
            tokio::spawn(async move {
                server.write_all(&bytes).await.unwrap();
                drop(server);
            });
            let mut fb = Framebuffer::new(4, 2).unwrap();
            assert_eq!(
                read_update(&mut client, &mut fb).await.err(),
                Some(expected),
                "{name}"
            );
            assert!(fb.bgrx.iter().all(|b| *b == 0), "{name} wrote pixels");
        }
        // Cut text, a bell and colour-map entries are skipped; the update after them is applied.
        let (mut client, mut server) = duplex(1 << 16);
        tokio::spawn(async move {
            let mut bytes = vec![2u8];
            bytes.extend_from_slice(&[3, 0, 0, 0, 0, 0, 0, 5]);
            bytes.extend_from_slice(b"hello");
            bytes.extend_from_slice(&[1, 0, 0, 0, 0, 2]);
            bytes.extend_from_slice(&[7u8; 12]);
            bytes.extend_from_slice(&update(&[(0, 0, 1, 1, 0, vec![5; 4])]));
            server.write_all(&bytes).await.unwrap();
        });
        let mut fb = Framebuffer::new(4, 2).unwrap();
        assert_eq!(read_update(&mut client, &mut fb).await, Ok(1));
        assert_eq!(&fb.bgrx[0..4], &[5; 4]);
    }
}
