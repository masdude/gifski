/*
 gifski pngquant-based GIF encoder
 © 2017 Kornel Lesiński

 This program is free software: you can redistribute it and/or modify
 it under the terms of the GNU Affero General Public License as
 published by the Free Software Foundation, either version 3 of the
 License, or (at your option) any later version.

 This program is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 GNU Affero General Public License for more details.

 You should have received a copy of the GNU Affero General Public License
 along with this program.  If not, see <https://www.gnu.org/licenses/>.
*/

extern crate rgb;
extern crate gif;
extern crate imgref;
extern crate imagequant;
extern crate resize;
extern crate lodepng;
extern crate gif_dispose;
extern crate rayon;
extern crate pbr;

#[macro_use] extern crate error_chain;
use gif::*;
use rgb::*;
use imgref::*;
use imagequant::*;

mod error;
pub use error::*;
mod ordqueue;
use ordqueue::*;
pub mod progress;
use progress::*;
pub mod c_api;

use std::path::PathBuf;
use std::io::prelude::*;
use std::sync::Arc;
use std::borrow::Cow;
use std::thread;

type DecodedImage = CatResult<(ImgVec<RGBA8>, u16)>;

#[derive(Copy, Clone)]
pub struct Settings {
    /// Resize to max this width if set
    pub width: Option<u32>,
    /// Resize to max this height if width is set. Note that aspect ratio is not preserved.
    pub height: Option<u32>,
    /// 1-100
    pub quality: u8,
    /// If true, looping is disabled
    pub once: bool,
    /// Lower quality, but faster encode
    pub fast: bool,
}

/// Collect frames that will be encoded
///
/// Note that writing will finish only when the collector is dropped.
/// Collect frames on another thread, or call `drop(collector)` before calling `writer.write()`!
pub struct Collector {
    width: Option<u32>,
    height: Option<u32>,
    queue: OrdQueue<DecodedImage>,
}

/// Perform GIF writing
pub struct Writer {
    queue_iter: Option<OrdQueueIter<DecodedImage>>,
    settings: Settings,
}

struct GIFFrame {
    image: ImgVec<u8>,
    pal: Vec<RGBA8>,
    delay: u16,
}

/// Encoder is initialized after first frame is decoded,
/// and this explains to Rust that writer `W` is used once for this.
enum WriteInitState<W: Write> {
    Uninit(W),
    Init(Encoder<W>),
}

/// Start new encoding
///
/// Encoding is multi-threaded, and the `Collector` and `Writer`
/// can be used on sepate threads.
pub fn new(settings: Settings) -> CatResult<(Collector, Writer)> {
    let (queue, queue_iter) = ordqueue::new(4);

    Ok((Collector {
        queue,
        width: settings.width,
        height: settings.height,
    }, Writer {
        queue_iter: Some(queue_iter),
        settings,
    }))
}

impl Collector {
    /// Frame index starts at 0. Set each frame only once, but you can set them in any order.
    /// Frame delay is in GIF units (1/100s).
    pub fn add_frame_rgba(&mut self, frame_index: usize, image: ImgVec<RGBA8>, delay: u16) -> CatResult<()> {
        self.queue.push(frame_index, Ok((Self::resized(image, self.width, self.height), delay)))
    }

    /// Read and decode a PNG file from disk. Frame index starts at 0. Frame delay is in GIF units (1/100s)
    pub fn add_frame_png_file(&mut self, frame_index: usize,  path: PathBuf, delay: u16) -> CatResult<()> {
        let width = self.width;
        let height = self.height;
        let image = lodepng::decode32_file(&path)
            .chain_err(|| format!("Can't load {}", path.display()))?;

        self.queue.push(frame_index, Ok((Self::resized(ImgVec::new(image.buffer, image.width, image.height), width, height), delay)))
    }

    fn resized(mut image: ImgVec<RGBA8>, width: Option<u32>, height: Option<u32>) -> ImgVec<RGBA8> {
        if let Some(width) = width {
            if image.width() != image.stride() {
                let mut contig = Vec::with_capacity(image.width()*image.height());
                contig.extend(image.rows().flat_map(|r| r.iter().cloned()));
                image = ImgVec::new(contig, image.width(), image.height());
            }
            let dst_width = (width as usize).min(image.width());
            let dst_height = height.map(|h| (h as usize).min(image.height())).unwrap_or(image.height() * dst_width / image.width());
            let mut r = resize::new(image.width(), image.height(), dst_width, dst_height, resize::Pixel::RGBA, resize::Type::Lanczos3);
            let mut dst = vec![RGBA::new(0,0,0,0); dst_width * dst_height];
            r.resize(image.buf.as_bytes(), dst.as_bytes_mut());
            ImgVec::new(dst, dst_width, dst_height)
        } else {
            image
        }
    }
}

/// Encode collected frames
impl Writer {

    /// `importance_map` is computed from previous and next frame.
    /// Improves quality of pixels visible for longer.
    /// Avoids wasting palette on pixels identical to the background.
    ///
    /// `background` is the previous frame.
    fn quantize(image: ImgRef<RGBA8>, importance_map: &[u8], background: Option<ImgRef<RGBA8>>, settings: &Settings) -> CatResult<(ImgVec<u8>, Vec<RGBA8>)> {
        let mut liq = Attributes::new();
        if settings.fast {
            liq.set_speed(10);
        }
        let quality = if background.is_some() { // not first frame
            settings.quality.into()
        } else {
            100 // the first frame is too important to ruin it
        };
        liq.set_quality(0, quality);
        let mut img = liq.new_image_stride(image.buf, image.width(), image.height(), image.stride(), 0.)?;
        img.set_importance_map(importance_map)?;
        if let Some(bg) = background {
            img.set_background(liq.new_image(bg.buf, bg.width(), bg.height(), 0.)?)?;
        }
        img.add_fixed_color(RGBA8::new(0,0,0,0));
        let mut res = liq.quantize(&img)?;
        res.set_dithering_level(0.5);

        let (pal, pal_img) = res.remapped(&mut img)?;
        debug_assert_eq!(img.width() * img.height(), pal_img.len());

        Ok((Img::new(pal_img, img.width(), img.height()), pal))
    }

    fn write_frames<W: Write + Send>(write_queue_iter: OrdQueueIter<Arc<GIFFrame>>, outfile: W, settings: &Settings, reporter: &mut ProgressReporter) -> CatResult<()> {
        let mut enc = WriteInitState::Uninit(outfile);

        for f in write_queue_iter {
            let GIFFrame {ref pal, ref image, delay} = *f;
            reporter.increase();

            let mut transparent_index = None;
            let mut pal_rgb = Vec::with_capacity(3 * pal.len());
            for (i, p) in pal.into_iter().enumerate() {
                if p.a == 0 {
                    transparent_index = Some(i as u8);
                }
                pal_rgb.extend_from_slice([p.rgb()].as_bytes());
            }

            enc = match enc {
                WriteInitState::Uninit(w) => {
                    let mut enc = Encoder::new(w, image.width() as u16, image.height() as u16, &[])?;
                    if !settings.once {
                        enc.write_extension(gif::ExtensionData::Repetitions(gif::Repeat::Infinite))?;
                    }
                    WriteInitState::Init(enc)
                },
                x => x,
            };
            let enc = match enc {
                WriteInitState::Init(ref mut r) => r,
                _ => unreachable!(),
            };

            enc.write_frame(&Frame {
                delay,
                dispose: DisposalMethod::Keep,
                transparent: transparent_index,
                needs_user_input: false,
                top: 0,
                left: 0,
                width: image.width() as u16,
                height: image.height() as u16,
                interlaced: false,
                palette: Some(pal_rgb),
                buffer: Cow::Borrowed(&image.buf),
            })?;
        }
        Ok(())
    }

    /// Start writing frames. This function will not return until `Collector` is dropped.
    ///
    /// `outfile` can be any writer, such as `File` or `&mut Vec`.
    ///
    /// `ProgressReporter.increase()` is called each time a new frame is being written.
    pub fn write<W: Write + Send>(mut self, outfile: W, reporter: &mut ProgressReporter) -> CatResult<()> {
        let (write_queue, write_queue_iter) = ordqueue::new(4);
        let queue_iter = self.queue_iter.take().unwrap();
        let settings = self.settings.clone();
        let make_thread = thread::spawn(move || {
            Self::make_frames(queue_iter, write_queue, &settings)
        });
        Self::write_frames(write_queue_iter, outfile, &self.settings, reporter)?;
        make_thread.join().unwrap()?;
        Ok(())
    }

    fn make_frames(queue_iter: OrdQueueIter<DecodedImage>, mut write_queue: OrdQueue<Arc<GIFFrame>>, settings: &Settings) -> CatResult<()> {
        let mut decode_iter = queue_iter.enumerate().map(|(i,tmp)| tmp.map(|(image, delay)|(i,image,delay)));

        let mut screen = None;
        let mut curr_frame = if let Some(a) = decode_iter.next() {
            Some(a?)
        } else {
            Err("Found no usable frames to encode")?
        };
        let mut importance_map = vec![255u8; curr_frame.as_ref().unwrap().1.buf.len()];
        let mut next_frame = if let Some(a) = decode_iter.next() {
            Some(a?)
        } else {
            None
        };

        while let Some((i, image, delay)) = curr_frame.take() {
            curr_frame = next_frame.take();
            next_frame = if let Some(a) = decode_iter.next() {
                Some(a?)
            } else {
                None
            };

            if let Some((_, ref next, _)) = next_frame {
                if next.width() != image.width() || next.height() != image.height() {
                    Err(format!("Frame {} has wrong size ({}×{}, expected {}×{})", i+1,
                        next.width(), next.height(), image.width(), image.height()))?;
                }

                debug_assert_eq!(next.width(), image.width());
                importance_map.clear();
                importance_map.extend(next.rows().zip(image.rows()).flat_map(|(a,b)| a.iter().cloned().zip(b.iter().cloned())).map(|(a,b)| {
                    // Even if next frame completely overwrites it, it's still somewhat important to display current one
                    // but pixels that will stay unchanged should have higher quality
                    255 - (colordiff(a,b) / (255*255*6/170)) as u8
                }));
            };

            if screen.is_none() {
                screen = Some(gif_dispose::Screen::new(image.width(), image.height(), RGBA8::new(0,0,0,0), None));
            }
            let screen = screen.as_mut().unwrap();

            let has_prev_frame = i > 0;
            if has_prev_frame {
                let q = 100 - settings.quality as u32;
                let min_diff = 80 + q * q;
                debug_assert_eq!(image.width(), screen.pixels.width());
                importance_map.chunks_mut(image.width()).zip(screen.pixels.rows().zip(image.rows()))
                .flat_map(|(px, (a,b))| {
                    px.iter_mut().zip(a.iter().cloned().zip(b.iter().cloned()))
                })
                .for_each(|(px, (a,b))| {
                    // TODO: try comparing with max-quality dithered non-transparent frame, but at half res to avoid dithering confusing the results
                    // and pick pixels/areas that are better left transparent?

                    let diff = colordiff(a,b);
                    // if pixels are close or identical, no weight on them
                    *px = if diff < min_diff {
                        0
                    } else {
                        // clip max value, since if something's different it doesn't matter how much, it has to be displayed anyway
                        // but multiply by previous map last, since it already decided non-max value
                        let t = diff / 32;
                        ((t * t).min(256) as u16 * u16::from(*px) / 256) as u8
                    }
                });
            }

            let (image8, image8_pal) = {
                let bg = if has_prev_frame {Some(screen.pixels.as_ref())} else {None};
                Self::quantize(image.as_ref(), &importance_map, bg, settings)?
            };

            let transparent_index = image8_pal.iter().position(|p| p.a == 0).map(|i| i as u8);
            let frame = Arc::new(GIFFrame {
                image: image8,
                pal: image8_pal,
                delay,
            });

            write_queue.push(i, frame.clone())?;
            screen.blit(Some(&frame.pal), gif::DisposalMethod::Keep, 0, 0, frame.image.as_ref(), transparent_index)?;
        }

        Ok(())
    }
}

#[inline]
fn colordiff(a: RGBA8, b: RGBA8) -> u32 {
    if a.a == 0 || b.a == 0 {
        return 255*255*6;
    }
    (i32::from(a.r as i16 - b.r as i16) * i32::from(a.r as i16 - b.r as i16)) as u32 * 2 +
    (i32::from(a.g as i16 - b.g as i16) * i32::from(a.g as i16 - b.g as i16)) as u32 * 3 +
    (i32::from(a.b as i16 - b.b as i16) * i32::from(a.b as i16 - b.b as i16)) as u32
}
