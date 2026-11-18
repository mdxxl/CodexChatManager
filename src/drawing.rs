use crate::language::trf;
use crate::wide;
use std::ptr::{null, null_mut};
use std::result::Result::Ok;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::{Gdi::*, GdiPlus::*};
use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, PRF_CLIENT, WM_PRINT};

pub const FG: u32 = 0xededed;
pub const MUTED: u32 = 0x8c8c8c;
pub const SUBTITLE: u32 = 0x969696;
pub const TINY: u32 = 0x646464;
pub const SECTION: u32 = 0x777777;

#[derive(Clone, Copy, Debug)]
pub struct Rect(pub i32, pub i32, pub i32, pub i32);

impl Rect {
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.0 && x < self.0 + self.2 && y >= self.1 && y < self.1 + self.3
    }

    pub fn visible_in(self, view: Rect) -> bool {
        self.1 < view.1 + view.3 && self.1 + self.3 > view.1
    }
}

#[derive(Clone, Copy)]
pub enum Glyph {
    Chat,
    History,
    Archive,
    Search,
    Refresh,
    Settings,
    Repair,
    Close = 9,
    Minimize,
    Maximize,
    Code = 14,
}

const ICON_SIZE: usize = 48;
const ICON_MASKS: &[u8] = include_bytes!("icons.alpha");

fn icon_mask(glyph: Glyph) -> &'static [u8] {
    let start = glyph as usize * ICON_SIZE * ICON_SIZE;
    &ICON_MASKS[start..start + ICON_SIZE * ICON_SIZE]
}

#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Center,
    Right,
}

pub fn colorref(rgb: u32) -> u32 {
    (rgb & 0xff) << 16 | rgb & 0xff00 | (rgb >> 16) & 0xff
}

pub fn font(size: i32, weight: i32, dpi: u32) -> HFONT {
    unsafe {
        CreateFontW(
            -(size * dpi as i32 + 48) / 96,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide("Segoe UI").as_ptr(),
        )
    }
}

pub struct Canvas {
    pub dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    dpi: u32,
    fonts: Vec<((i32, i32), *mut GpFont)>,
}

impl Canvas {
    pub fn new(dest: HDC, width: i32, height: i32, dpi: u32) -> Self {
        unsafe {
            let dc = CreateCompatibleDC(dest);
            let bitmap = CreateCompatibleBitmap(dest, width.max(1), height.max(1));
            let previous = SelectObject(dc, bitmap);
            SetBkMode(dc, TRANSPARENT as i32);
            Self {
                dc,
                bitmap,
                previous,
                dpi,
                fonts: Vec::new(),
            }
        }
    }

    pub fn px(&self, value: i32) -> i32 {
        (value * self.dpi as i32 + if value < 0 { -48 } else { 48 }) / 96
    }

    pub fn physical(&self, r: Rect) -> RECT {
        RECT {
            left: self.px(r.0),
            top: self.px(r.1),
            right: self.px(r.0 + r.2),
            bottom: self.px(r.1 + r.3),
        }
    }

    pub fn fill(&self, r: Rect, color: u32) {
        unsafe {
            let brush = CreateSolidBrush(colorref(color));
            FillRect(self.dc, &self.physical(r), brush);
            DeleteObject(brush);
        }
    }

    fn shape(&self, draw: impl FnOnce(*mut GpGraphics)) {
        unsafe {
            let mut graphics = null_mut();
            if GdipCreateFromHDC(self.dc, &mut graphics) == 0 {
                GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias8x8);
                GdipSetPixelOffsetMode(graphics, PixelOffsetModeHalf);
                draw(graphics);
                GdipDeleteGraphics(graphics);
            }
        }
    }

    pub fn rounded(&self, r: Rect, radius: i32, fill: Option<u32>, stroke: Option<u32>) {
        let p = self.physical(r);
        let d = (self.px(radius) * 2).max(2) as f32;
        let (x, y, w, h) = (
            p.left as f32 + 0.5,
            p.top as f32 + 0.5,
            (p.right - p.left) as f32 - 1.0,
            (p.bottom - p.top) as f32 - 1.0,
        );
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let d = d.min(w).min(h);
        self.shape(|graphics| unsafe {
            let mut path = null_mut();
            GdipCreatePath(FillModeAlternate, &mut path);
            if path.is_null() {
                return;
            }
            for (x, y, angle) in [
                (x, y, 180.0),
                (x + w - d, y, 270.0),
                (x + w - d, y + h - d, 0.0),
                (x, y + h - d, 90.0),
            ] {
                GdipAddPathArc(path, x, y, d, d, angle, 90.0);
            }
            GdipClosePathFigure(path);
            if let Some(color) = fill {
                let mut brush = null_mut();
                GdipCreateSolidFill(0xff000000 | color, &mut brush);
                GdipFillPath(graphics, brush.cast(), path);
                GdipDeleteBrush(brush.cast());
            }
            if let Some(color) = stroke {
                let mut pen = null_mut();
                GdipCreatePen1(
                    0xff000000 | color,
                    self.dpi as f32 / 96.0,
                    UnitPixel,
                    &mut pen,
                );
                GdipDrawPath(graphics, pen, path);
                GdipDeletePen(pen);
            }
            GdipDeletePath(path);
        });
    }

    pub fn check_mark(&self, x: i32, y: i32) {
        let f = self.dpi as f32 / 96.0;
        self.shape(|graphics| unsafe {
            let mut pen = null_mut();
            GdipCreatePen1(0xff202020, 1.8 * f, UnitPixel, &mut pen);
            GdipSetPenStartCap(pen, LineCapRound);
            GdipSetPenEndCap(pen, LineCapRound);
            let points = [(x + 4, y + 8), (x + 7, y + 11), (x + 13, y + 5)].map(|(x, y)| PointF {
                X: x as f32 * f,
                Y: y as f32 * f,
            });
            GdipDrawLines(graphics, pen, points.as_ptr(), points.len() as i32);
            GdipDeletePen(pen);
        });
    }

    pub fn icon(&self, glyph: Glyph, r: Rect, color: u32) {
        const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026200a;
        let mut pixels = [0u32; ICON_SIZE * ICON_SIZE];
        for (pixel, alpha) in pixels.iter_mut().zip(icon_mask(glyph)) {
            *pixel = (*alpha as u32) << 24 | color;
        }
        let p = self.physical(r);
        self.shape(|graphics| unsafe {
            let mut bitmap = null_mut();
            GdipCreateBitmapFromScan0(
                ICON_SIZE as i32,
                ICON_SIZE as i32,
                (ICON_SIZE * 4) as i32,
                PIXEL_FORMAT_32BPP_ARGB,
                pixels.as_ptr().cast(),
                &mut bitmap,
            );
            if !bitmap.is_null() {
                GdipSetInterpolationMode(graphics, InterpolationModeHighQualityBicubic);
                GdipDrawImageRectI(
                    graphics,
                    bitmap.cast(),
                    p.left,
                    p.top,
                    p.right - p.left,
                    p.bottom - p.top,
                );
                GdipDisposeImage(bitmap.cast());
            }
        });
    }

    fn canvas_font(&mut self, size: i32, weight: i32) -> *mut GpFont {
        let key = (size, weight);
        self.fonts
            .iter()
            .find(|(stored, _)| *stored == key)
            .map(|(_, font)| *font)
            .unwrap_or_else(|| {
                let face = if weight >= 600 {
                    "Segoe UI Semibold"
                } else {
                    "Segoe UI"
                };
                let mut family = null_mut();
                let mut font = null_mut();
                unsafe {
                    let mut style = FontStyleRegular;
                    if GdipCreateFontFamilyFromName(wide(face).as_ptr(), null_mut(), &mut family)
                        != 0
                    {
                        GdipCreateFontFamilyFromName(
                            wide("Segoe UI").as_ptr(),
                            null_mut(),
                            &mut family,
                        );
                        if weight >= 600 {
                            style = FontStyleBold;
                        }
                    }
                    if !family.is_null() {
                        GdipCreateFont(
                            family,
                            size as f32 * self.dpi as f32 / 96.0,
                            style,
                            UnitPixel,
                            &mut font,
                        );
                        GdipDeleteFontFamily(family);
                    }
                }
                self.fonts.push((key, font));
                font
            })
    }

    fn text_rect(&self, r: Rect) -> RectF {
        let p = self.physical(r);
        RectF {
            X: p.left as f32,
            Y: p.top as f32,
            Width: (p.right - p.left) as f32,
            Height: (p.bottom - p.top) as f32,
        }
    }

    fn draw_text(&self, draw: impl FnOnce(*mut GpGraphics)) {
        self.shape(|graphics| unsafe {
            // Text is drawn onto the opaque back buffer at the window's monitor DPI.
            GdipSetPixelOffsetMode(graphics, PixelOffsetModeNone);
            GdipSetTextRenderingHint(graphics, TextRenderingHintClearTypeGridFit);
            draw(graphics);
        });
    }

    pub fn text(&mut self, value: &str, r: Rect, size: i32, weight: i32, color: u32, align: Align) {
        let font = self.canvas_font(size, weight);
        if value.is_empty() || font.is_null() {
            return;
        }
        let rect = self.text_rect(r);
        let text = wide(value);
        self.draw_text(|graphics| unsafe {
            let mut format = null_mut();
            let mut ink = null_mut();
            GdipStringFormatGetGenericTypographic(&mut format);
            GdipCreateSolidFill(0xff000000 | color, &mut ink);
            GdipSetStringFormatFlags(
                format,
                StringFormatFlagsNoFitBlackBox | StringFormatFlagsNoWrap,
            );
            GdipSetStringFormatAlign(
                format,
                match align {
                    Align::Left => StringAlignmentNear,
                    Align::Center => StringAlignmentCenter,
                    Align::Right => StringAlignmentFar,
                },
            );
            GdipSetStringFormatLineAlign(format, StringAlignmentCenter);
            GdipSetStringFormatTrimming(format, StringTrimmingEllipsisCharacter);
            GdipDrawString(
                graphics,
                text.as_ptr(),
                text.len() as i32 - 1,
                font,
                &rect,
                format,
                ink.cast(),
            );
            GdipDeleteBrush(ink.cast());
            GdipDeleteStringFormat(format);
        });
    }

    pub fn wrapped(&mut self, value: &str, r: Rect, size: i32, color: u32, measure: bool) -> i32 {
        let font = self.canvas_font(size, 400);
        let mut rect = self.text_rect(r);
        let text = wide(value);
        if font.is_null() {
            return size;
        }
        let mut bounds = RectF {
            X: 0.0,
            Y: 0.0,
            Width: 0.0,
            Height: 0.0,
        };
        self.draw_text(|graphics| unsafe {
            let mut format = null_mut();
            GdipStringFormatGetGenericTypographic(&mut format);
            GdipSetStringFormatFlags(format, StringFormatFlagsNoFitBlackBox);
            if measure {
                rect.Height = 100000.0;
                GdipMeasureString(
                    graphics,
                    text.as_ptr(),
                    text.len() as i32 - 1,
                    font,
                    &rect,
                    format,
                    &mut bounds,
                    null_mut(),
                    null_mut(),
                );
            } else {
                let mut ink = null_mut();
                GdipCreateSolidFill(0xff000000 | color, &mut ink);
                GdipDrawString(
                    graphics,
                    text.as_ptr(),
                    text.len() as i32 - 1,
                    font,
                    &rect,
                    format,
                    ink.cast(),
                );
                GdipDeleteBrush(ink.cast());
            }
            GdipDeleteStringFormat(format);
        });
        ((bounds.Height * 96.0 / self.dpi as f32).ceil() as i32).max(size)
    }

    pub fn present(&self, target: HDC, width: i32, height: i32) {
        unsafe {
            BitBlt(target, 0, 0, width, height, self.dc, 0, 0, SRCCOPY);
        }
    }

    pub fn faded(&mut self, area: Rect, opacity: u8, draw: impl FnOnce(&mut Self)) {
        if opacity == 0 {
            return;
        }
        if opacity == 255 {
            draw(self);
            return;
        }
        let r = self.physical(area);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let background = Self::new(self.dc, w, h, self.dpi);
        unsafe {
            BitBlt(background.dc, 0, 0, w, h, self.dc, r.left, r.top, SRCCOPY);
        }
        draw(self);
        unsafe {
            GdiAlphaBlend(
                self.dc,
                r.left,
                r.top,
                w,
                h,
                background.dc,
                0,
                0,
                w,
                h,
                BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255 - opacity,
                    AlphaFormat: 0,
                },
            );
        }
    }

    pub fn paint_control(&self, control: HWND, area: Rect) {
        let r = self.physical(area);
        unsafe {
            let saved = SaveDC(self.dc);
            if saved == 0 {
                return;
            }
            IntersectClipRect(self.dc, r.left, r.top, r.right, r.bottom);
            SetViewportOrgEx(self.dc, r.left, r.top, null_mut());
            SendMessageW(control, WM_PRINT, self.dc as usize, PRF_CLIENT as isize);
            RestoreDC(self.dc, saved);
        }
    }

    pub fn save_png(&self, path: &std::path::Path, r: Rect) -> crate::Result<()> {
        let png = GUID {
            data1: 0x557cf406,
            data2: 0x1a04,
            data3: 0x11d3,
            data4: [0x9a, 0x73, 0x00, 0x00, 0xf8, 0x1e, 0xf3, 0x2e],
        };
        let r = self.physical(r);
        let cropped = Self::new(self.dc, r.right - r.left, r.bottom - r.top, 96);
        unsafe {
            BitBlt(
                cropped.dc,
                0,
                0,
                r.right - r.left,
                r.bottom - r.top,
                self.dc,
                r.left,
                r.top,
                SRCCOPY,
            );
            SelectObject(cropped.dc, cropped.previous);
            let mut bitmap = null_mut();
            let source = GdipCreateBitmapFromHBITMAP(cropped.bitmap, null_mut(), &mut bitmap);
            if source != 0 {
                return Err(crate::err(trf!(
                    "Не удалось создать снимок окна ({source})",
                    "Could not capture the window ({source})"
                )));
            }
            let status = GdipSaveImageToFile(bitmap.cast(), wide(path).as_ptr(), &png, null());
            GdipDisposeImage(bitmap.cast());
            if status != 0 {
                return Err(crate::err(trf!(
                    "Не удалось сохранить PNG ({status})",
                    "Could not save the PNG ({status})"
                )));
            }
        }
        Ok(())
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            for (_, font) in self.fonts.drain(..) {
                if !font.is_null() {
                    GdipDeleteFont(font);
                }
            }
            DeleteDC(self.dc);
        }
    }
}
