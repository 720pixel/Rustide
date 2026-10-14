use egui::epaint::{ClippedPrimitive, ImageData, Mesh, Primitive, TextureId};
use egui::{Color32, Rect, TexturesDelta};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Painter {
    textures: HashMap<TextureId, Texture>,
    pixels: Vec<u32>,
}

struct Texture {
    size: [usize; 2],
    pixels: Vec<Color32>,
}

impl Painter {
    pub(super) fn paint(
        &mut self,
        size: [usize; 2],
        scale: f32,
        primitives: &[ClippedPrimitive],
        textures: &TexturesDelta,
        clear: Color32,
    ) -> &[u32] {
        for (id, delta) in &textures.set {
            let ImageData::Color(image) = &delta.image;
            if let Some([x, y]) = delta.pos {
                if let Some(texture) = self.textures.get_mut(id) {
                    for row in 0..image.size[1] {
                        if y + row >= texture.size[1] || x >= texture.size[0] {
                            break;
                        }
                        let len = image.size[0].min(texture.size[0] - x);
                        let start = (y + row) * texture.size[0] + x;
                        texture.pixels[start..start + len].copy_from_slice(
                            &image.pixels[row * image.size[0]..row * image.size[0] + len],
                        );
                    }
                }
            } else {
                self.textures.insert(
                    *id,
                    Texture {
                        size: image.size,
                        pixels: image.pixels.clone(),
                    },
                );
            }
        }
        self.pixels.resize(size[0] * size[1], 0);
        self.pixels.fill(rgb(clear.r(), clear.g(), clear.b()));
        for primitive in primitives {
            if let Primitive::Mesh(mesh) = &primitive.primitive
                && let Some(texture) = self.textures.get(&mesh.texture_id)
            {
                rasterize(
                    &mut self.pixels,
                    size,
                    scale,
                    primitive.clip_rect,
                    mesh,
                    texture,
                );
            }
        }
        for id in &textures.free {
            self.textures.remove(id);
        }
        &self.pixels
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

fn rasterize(
    pixels: &mut [u32],
    size: [usize; 2],
    scale: f32,
    clip: Rect,
    mesh: &Mesh,
    texture: &Texture,
) {
    let clip_min = [
        (clip.min.x * scale).floor().max(0.0) as usize,
        (clip.min.y * scale).floor().max(0.0) as usize,
    ];
    let clip_max = [
        (clip.max.x * scale).ceil().max(0.0) as usize,
        (clip.max.y * scale).ceil().max(0.0) as usize,
    ];
    for indices in mesh.indices.as_chunks::<3>().0 {
        let mut v = [
            mesh.vertices[indices[0] as usize],
            mesh.vertices[indices[1] as usize],
            mesh.vertices[indices[2] as usize],
        ];
        let mut p = v.map(|vertex| [vertex.pos.x * scale, vertex.pos.y * scale]);
        let mut area = edge(p[0], p[1], p[2]);
        if area.abs() < 1.0e-6 {
            continue;
        }
        if area < 0.0 {
            v.swap(1, 2);
            p.swap(1, 2);
            area = -area;
        }
        let min_x = p
            .iter()
            .map(|point| point[0])
            .fold(f32::INFINITY, f32::min)
            .floor()
            .max(0.0) as usize;
        let min_y = p
            .iter()
            .map(|point| point[1])
            .fold(f32::INFINITY, f32::min)
            .floor()
            .max(0.0) as usize;
        let max_x = p
            .iter()
            .map(|point| point[0])
            .fold(0.0, f32::max)
            .ceil()
            .max(0.0) as usize;
        let max_y = p
            .iter()
            .map(|point| point[1])
            .fold(0.0, f32::max)
            .ceil()
            .max(0.0) as usize;
        let min_x = min_x.max(clip_min[0]).min(size[0]);
        let min_y = min_y.max(clip_min[1]).min(size[1]);
        let max_x = max_x.min(clip_max[0]).min(size[0]);
        let max_y = max_y.min(clip_max[1]).min(size[1]);
        let edges = [(p[1], p[2]), (p[2], p[0]), (p[0], p[1])];
        let inclusive = edges.map(|(a, b)| b[1] < a[1] || (b[1] == a[1] && b[0] > a[0]));
        let inverse_area = 1.0 / area;
        let color = v.map(|vertex| vertex.color.to_array().map(f32::from));
        let uniform_color = v[0].color == v[1].color && v[0].color == v[2].color;
        let flat_source: Option<[f32; 4]> =
            if uniform_color && v[0].uv == v[1].uv && v[0].uv == v[2].uv {
                let sampled = texture.sample(v[0].uv.x, v[0].uv.y);
                Some(std::array::from_fn(|channel| {
                    color[0][channel] * sampled[channel] / 255.0
                }))
            } else {
                None
            };
        if flat_source.is_some_and(|source| source[3] == 0.0) {
            continue;
        }
        let flat_pixel = flat_source
            .filter(|source| source[3] == 255.0)
            .map(|source| {
                rgb(
                    source[0].round() as u8,
                    source[1].round() as u8,
                    source[2].round() as u8,
                )
            });
        for y in min_y..max_y {
            for x in min_x..max_x {
                let point = [x as f32 + 0.5, y as f32 + 0.5];
                let e = edges.map(|(a, b)| edge(a, b, point));
                if (0..3).any(|i| e[i] < 0.0 || (e[i] == 0.0 && !inclusive[i])) {
                    continue;
                }
                if let Some(pixel) = flat_pixel {
                    pixels[y * size[0] + x] = pixel;
                    continue;
                }
                let source = flat_source.unwrap_or_else(|| {
                    let weights = e.map(|value| value * inverse_area);
                    let u =
                        weights[0] * v[0].uv.x + weights[1] * v[1].uv.x + weights[2] * v[2].uv.x;
                    let t =
                        weights[0] * v[0].uv.y + weights[1] * v[1].uv.y + weights[2] * v[2].uv.y;
                    let sampled = texture.sample(u, t);
                    std::array::from_fn(|channel| {
                        let vertex = if uniform_color {
                            color[0][channel]
                        } else {
                            weights[0] * color[0][channel]
                                + weights[1] * color[1][channel]
                                + weights[2] * color[2][channel]
                        };
                        vertex * sampled[channel] / 255.0
                    })
                });
                let destination = pixels[y * size[0] + x];
                let remaining = 1.0 - source[3] / 255.0;
                let r = (source[0] + ((destination >> 16) & 255) as f32 * remaining)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                let g = (source[1] + ((destination >> 8) & 255) as f32 * remaining)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                let b = (source[2] + (destination & 255) as f32 * remaining)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                pixels[y * size[0] + x] = rgb(r, g, b);
            }
        }
    }
}

fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

impl Texture {
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        if self.size[0] == 0 || self.size[1] == 0 {
            return [0.0; 4];
        }
        let x = (u * self.size[0] as f32 - 0.5).clamp(0.0, (self.size[0] - 1) as f32);
        let y = (v * self.size[1] as f32 - 0.5).clamp(0.0, (self.size[1] - 1) as f32);
        let x0 = x.floor() as usize;
        let y0 = y.floor() as usize;
        let x1 = (x0 + 1).min(self.size[0] - 1);
        let y1 = (y0 + 1).min(self.size[1] - 1);
        let tx = x - x0 as f32;
        let ty = y - y0 as f32;
        let colors = [
            self.pixels[y0 * self.size[0] + x0],
            self.pixels[y0 * self.size[0] + x1],
            self.pixels[y1 * self.size[0] + x0],
            self.pixels[y1 * self.size[0] + x1],
        ];
        let colors = colors.map(|color| color.to_array().map(f32::from));
        std::array::from_fn(|channel| {
            let top = colors[0][channel] * (1.0 - tx) + colors[1][channel] * tx;
            let bottom = colors[2][channel] * (1.0 - tx) + colors[3][channel] * tx;
            top * (1.0 - ty) + bottom * ty
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{ImageDelta, Vertex};
    use egui::{ColorImage, TextureOptions, pos2};
    use std::sync::Arc;

    fn white_texture() -> TexturesDelta {
        TexturesDelta {
            set: vec![(
                TextureId::Managed(0),
                ImageDelta::full(
                    Arc::new(ColorImage::filled([1, 1], Color32::WHITE)),
                    TextureOptions::LINEAR,
                ),
            )],
            free: vec![],
        }
    }

    fn quad(color: Color32, clip: Rect) -> ClippedPrimitive {
        let vertices = [
            pos2(0.0, 0.0),
            pos2(4.0, 0.0),
            pos2(4.0, 4.0),
            pos2(0.0, 4.0),
        ]
        .map(|pos| Vertex {
            pos,
            uv: pos2(0.5, 0.5),
            color,
        })
        .to_vec();
        let mesh = Mesh {
            vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            ..Default::default()
        };
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    #[test]
    fn shared_triangle_edges_blend_only_once() {
        let mut painter = Painter::default();
        let pixels = painter.paint(
            [4, 4],
            1.0,
            &[quad(
                Color32::from_rgba_premultiplied(128, 0, 0, 128),
                Rect::EVERYTHING,
            )],
            &white_texture(),
            Color32::BLACK,
        );
        assert_eq!(pixels, &[0x0080_0000; 16]);
    }

    #[test]
    fn clips_and_scales_meshes() {
        let mut painter = Painter::default();
        let pixels = painter.paint(
            [8, 8],
            2.0,
            &[quad(
                Color32::WHITE,
                Rect::from_min_max(pos2(1.0, 1.0), pos2(3.0, 3.0)),
            )],
            &white_texture(),
            Color32::BLACK,
        );
        assert_eq!(
            pixels.iter().filter(|pixel| **pixel == 0x00ff_ffff).count(),
            16
        );
        assert_eq!(pixels[2 * 8 + 2], 0x00ff_ffff);
        assert_eq!(pixels[8 + 1], 0);
    }

    #[test]
    fn bilinear_texture_sampling_interpolates_channels() {
        let texture = Texture {
            size: [2, 2],
            pixels: vec![Color32::RED, Color32::GREEN, Color32::BLUE, Color32::WHITE],
        };
        assert_eq!(texture.sample(0.5, 0.5), [127.5, 127.5, 127.5, 255.0]);
        assert_eq!(texture.sample(0.0, 0.0), [255.0, 0.0, 0.0, 255.0]);
    }

    #[test]
    fn partial_texture_updates_and_free_are_honored() {
        let mut painter = Painter::default();
        painter.paint([1, 1], 1.0, &[], &white_texture(), Color32::BLACK);
        let delta = TexturesDelta {
            set: vec![(
                TextureId::Managed(0),
                ImageDelta::partial(
                    [0, 0],
                    Arc::new(ColorImage::filled([1, 1], Color32::BLUE)),
                    TextureOptions::LINEAR,
                ),
            )],
            free: vec![],
        };
        assert_eq!(
            painter.paint(
                [1, 1],
                1.0,
                &[quad(Color32::WHITE, Rect::EVERYTHING)],
                &delta,
                Color32::BLACK
            ),
            &[0x0000_00ff]
        );
        painter.paint(
            [1, 1],
            1.0,
            &[],
            &TexturesDelta {
                set: vec![],
                free: vec![TextureId::Managed(0)],
            },
            Color32::BLACK,
        );
        assert!(painter.textures.is_empty());
    }
}
