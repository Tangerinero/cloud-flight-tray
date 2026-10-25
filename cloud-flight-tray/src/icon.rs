use image::{Rgba, RgbaImage};
use tray_icon::Icon;

const SIZE: u32 = 32;

const TRANSPARENT: Rgba<u8> = Rgba([0, 0, 0, 0]);
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 230]);
const GREY: Rgba<u8> = Rgba([160, 160, 160, 230]);

enum Label {
    Dashes,
    Number(u8),
}

const FONT: [[u8; 5]; 11] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b001, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
    [0b000, 0b000, 0b111, 0b000, 0b000], // -
];

#[derive(Clone, Copy)]
pub enum IconKind {
    Disconnected,
    HeadsetOff,
    Charging { percent: Option<u8> },
    Battery { percent: u8 },
}

pub fn make_icon(kind: IconKind) -> Icon {
    let rgba = render(kind);
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid 32x32 rgba icon")
}

fn render(kind: IconKind) -> Vec<u8> {
    let mut img = RgbaImage::from_pixel(SIZE, SIZE, TRANSPARENT);
    let (fill, outline, label, bolt) = match kind {
        IconKind::Disconnected => (GREY, GREY, Label::Dashes, false),
        IconKind::HeadsetOff => (GREY, GREY, Label::Dashes, false),
        IconKind::Charging { percent } => {
            let pct = percent.unwrap_or(0);
            (
                level_color(pct.max(1)),
                WHITE,
                percent.map(Label::Number).unwrap_or(Label::Dashes),
                true,
            )
        }
        IconKind::Battery { percent } => {
            (level_color(percent), WHITE, Label::Number(percent), false)
        }
    };

    draw_battery_body(
        &mut img,
        outline,
        fill,
        match kind {
            IconKind::Battery { percent } => percent,
            IconKind::Charging { percent } => percent.unwrap_or(0),
            _ => 0,
        },
    );

    match label {
        Label::Dashes => {
            draw_glyph(&mut img, 8, 13, 10, GREY);
            draw_glyph(&mut img, 18, 13, 10, GREY);
        }
        Label::Number(pct) => draw_number(&mut img, pct, WHITE),
    }

    if bolt {
        draw_bolt(&mut img);
    }

    img.into_raw()
}

fn level_color(percent: u8) -> Rgba<u8> {
    if percent <= 20 {
        Rgba([220, 60, 50, 230])
    } else if percent <= 50 {
        Rgba([230, 180, 40, 230])
    } else {
        Rgba([70, 190, 80, 230])
    }
}

fn draw_battery_body(img: &mut RgbaImage, outline: Rgba<u8>, fill: Rgba<u8>, percent: u8) {
    fill_rect(img, 3, 8, 24, 16, outline);
    fill_rect(img, 5, 10, 20, 12, TRANSPARENT);
    fill_rect(img, 27, 12, 3, 8, outline);

    let inner_w = 18u32;
    let filled = ((percent as u32).min(100) * inner_w / 100).max(if percent > 0 { 1 } else { 0 });
    if filled > 0 {
        fill_rect(img, 6, 11, filled, 10, fill);
    }
}

fn draw_number(img: &mut RgbaImage, value: u8, color: Rgba<u8>) {
    let digits: Vec<u8> = if value >= 100 {
        vec![1, 0, 0]
    } else if value >= 10 {
        vec![value / 10, value % 10]
    } else {
        vec![value]
    };
    let width = digits.len() as i32 * 4 - 1;
    let start_x = (SIZE as i32 - width) / 2;
    for (i, d) in digits.into_iter().enumerate() {
        draw_glyph(img, start_x + i as i32 * 4, 13, d, color);
    }
}

fn draw_glyph(img: &mut RgbaImage, x: i32, y: i32, glyph: u8, color: Rgba<u8>) {
    let bits = FONT[glyph as usize];
    for (row, row_bits) in bits.iter().enumerate() {
        for col in 0..3 {
            if row_bits & (1 << (2 - col)) != 0 {
                put(img, x + col, y + row as i32, color);
            }
        }
    }
}

fn draw_bolt(img: &mut RgbaImage) {
    let c = Rgba([255, 230, 80, 255]);
    for (x, y) in [
        (22, 6),
        (21, 7),
        (20, 8),
        (19, 9),
        (21, 9),
        (20, 10),
        (19, 11),
        (18, 12),
    ] {
        put(img, x, y, c);
    }
}

fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    for py in y..y + h {
        for px in x..x + w {
            img.put_pixel(px, py, color);
        }
    }
}

fn put(img: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < SIZE && (y as u32) < SIZE {
        img.put_pixel(x as u32, y as u32, color);
    }
}
