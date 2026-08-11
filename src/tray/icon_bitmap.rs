pub(super) fn render(color: u32) -> ([u8; 32], [u8; 1024]) {
    let mut and_mask = [0xffu8; 32];
    let mut xor = [0u8; 1024];
    draw_icon_line(&mut and_mask, &mut xor, 3, 12, 7, 8, color);
    draw_icon_line(&mut and_mask, &mut xor, 7, 8, 7, 4, color);
    draw_icon_line(&mut and_mask, &mut xor, 7, 4, 13, 4, color);
    draw_icon_line(&mut and_mask, &mut xor, 11, 2, 13, 4, color);
    draw_icon_line(&mut and_mask, &mut xor, 11, 6, 13, 4, color);
    for (x, y) in [(3, 12), (7, 8), (7, 4)] {
        draw_icon_node(&mut and_mask, &mut xor, x, y, color);
    }
    (and_mask, xor)
}

fn set_icon_pixel(and_mask: &mut [u8; 32], xor: &mut [u8; 1024], x: i32, y: i32, color: u32) {
    if !(0..16).contains(&x) || !(0..16).contains(&y) {
        return;
    }
    let row = (15 - y) as usize;
    let x = x as usize;
    and_mask[row * 2 + x / 8] &= !(0x80 >> (x % 8));
    let offset = (row * 16 + x) * 4;
    xor[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
}

fn draw_icon_line(
    and_mask: &mut [u8; 32],
    xor: &mut [u8; 1024],
    mut x0: i32,
    mut y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        set_icon_pixel(and_mask, xor, x0, y0, color);
        set_icon_pixel(and_mask, xor, x0, y0 + 1, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x0 += sx
        }
        if twice <= dx {
            error += dx;
            y0 += sy
        }
    }
}

fn draw_icon_node(and_mask: &mut [u8; 32], xor: &mut [u8; 1024], x: i32, y: i32, color: u32) {
    for (dx, dy) in [
        (0, -2),
        (-1, -1),
        (0, -1),
        (1, -1),
        (-2, 0),
        (-1, 0),
        (0, 0),
        (1, 0),
        (2, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
        (0, 2),
    ] {
        set_icon_pixel(and_mask, xor, x + dx, y + dy, 0x00_ff_ff_ff)
    }
    set_icon_pixel(and_mask, xor, x, y, color);
}
