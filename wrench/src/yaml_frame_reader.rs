/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use app_units::Au;
use clap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use webrender_traits::*;
use yaml_helper::YamlHelper;
use yaml_rust::{Yaml, YamlLoader};

use wrench::{Wrench, WrenchThing, layout_simple_ascii};
use {WHITE_COLOR, PLATFORM_DEFAULT_FACE_NAME};

pub struct YamlFrameReader {
    frame_built: bool,
    yaml_path: PathBuf,
    aux_dir: PathBuf,
    frame_count: u32,

    builder: Option<DisplayListBuilder>,

    queue_depth: u32,
}

impl YamlFrameReader {
    pub fn new(yaml_path: &Path) -> YamlFrameReader {
        YamlFrameReader {
            frame_built: false,
            yaml_path: yaml_path.to_owned(),
            aux_dir: yaml_path.parent().unwrap().to_owned(),
            frame_count: 0,

            builder: None,

            queue_depth: 1,
        }
    }

    pub fn new_from_args(args: &clap::ArgMatches) -> YamlFrameReader {
        let yaml_file = args.value_of("INPUT").map(|s| PathBuf::from(s)).unwrap();

        let mut y = YamlFrameReader::new(&yaml_file);
        y.queue_depth = args.value_of("queue").map(|s| s.parse::<u32>().unwrap()).unwrap_or(1);
        y
    }

    pub fn builder<'a>(&'a mut self) -> &'a mut DisplayListBuilder {
        self.builder.as_mut().unwrap()
    }

    pub fn build(&mut self, wrench: &mut Wrench) {
        let mut file = File::open(&self.yaml_path).unwrap();
        let mut src = String::new();
        file.read_to_string(&mut src).unwrap();

        let mut yaml_doc = YamlLoader::load_from_str(&src).expect("Failed to parse YAML file");
        assert!(yaml_doc.len() == 1);

        let yaml = yaml_doc.pop().unwrap();
        if yaml["root"].is_badvalue() {
            panic!("Missing root stacking context");
        }
        self.add_stacking_context_from_yaml(wrench, &yaml["root"]);
    }

    fn handle_rect(&mut self, wrench: &mut Wrench, clip_region: &ClipRegion, item: &Yaml)
    {
        let rect = item[if item["type"].is_badvalue() { "rect" } else { "bounds" }]
            .as_rect().expect("rect type must have bounds");
        let color = item["color"].as_colorf().unwrap_or(*WHITE_COLOR);

        let builder = self.builder();
        let clip = item["clip"].as_clip_region(builder).unwrap_or(*clip_region);
        builder.push_rect(rect, clip, color);
    }

    fn handle_image(&mut self, wrench: &mut Wrench, clip_region: &ClipRegion, item: &Yaml)
    {
        let filename = item[if item["type"].is_badvalue() { "image" } else { "src" }].as_str().unwrap();
        let mut file = self.aux_dir.clone();
        file.push(filename);
        let (image_key, image_dims) = wrench.add_or_get_image(&file);

        let bounds_raws = item["bounds"].as_vec_f32().unwrap();
        let bounds = if bounds_raws.len() == 2 {
            LayoutRect::new(LayoutPoint::new(bounds_raws[0], bounds_raws[1]),
                            image_dims)
        } else if bounds_raws.len() == 4 {
            LayoutRect::new(LayoutPoint::new(bounds_raws[0], bounds_raws[1]),
                            LayoutSize::new(bounds_raws[2], bounds_raws[3]))
        } else {
            panic!("image expected 2 or 4 values in bounds, got '{:?}'", item["bounds"]);
        };

        let clip = item["clip"].as_clip_region(self.builder.as_mut().unwrap())
            .unwrap_or(*clip_region);
        let stretch_size = item["stretch_size"].as_size()
            .unwrap_or(image_dims);
        let tile_spacing = item["tile_spacing"].as_size()
            .unwrap_or(LayoutSize::new(0.0, 0.0));
        let rendering = match item["rendering"].as_str() {
            Some("auto") | None => ImageRendering::Auto,
            Some("crisp_edges") => ImageRendering::CrispEdges,
            Some("pixelated") => ImageRendering::Pixelated,
            Some(_) => panic!("ImageRendering can be auto, crisp_edges, or pixelated -- got {:?}", item),
        };
        self.builder().push_image(bounds, clip, stretch_size, tile_spacing, rendering, image_key);
    }

    fn handle_text(&mut self, wrench: &mut Wrench, clip_region: &ClipRegion, item: &Yaml)
    {
        let size = item["size"].as_pt_to_au().unwrap_or(Au::from_f32_px(16.0));
        let color = item["color"].as_colorf().unwrap_or(*WHITE_COLOR);
        let blur_radius = item["blur_radius"].as_px_to_au().unwrap_or(Au::from_f32_px(0.0));

        let (font_key, native_key) = if !item["family"].is_badvalue() {
            wrench.font_key_from_yaml_table(item)
        } else if !item["font"].is_badvalue() {
            let font_file = item["font"].as_str().unwrap();
            let mut file = File::open(PathBuf::from(font_file)).expect("Couldn't open font file");
            let mut bytes = vec![];
            file.read_to_end(&mut bytes).expect("failed to read font file");
            wrench.font_key_from_bytes(bytes)
        } else {
            wrench.font_key_from_name(&*PLATFORM_DEFAULT_FACE_NAME)
        };

        if item["glyphs"].is_badvalue() && item["text"].is_badvalue() {
            panic!("text item had neither text nor glyphs!");
        }

        let (glyphs, rect) = if item["text"].is_badvalue() {
            // if glyphs are specified, then the glyph positions can have the
            // origin baked in.
            let origin = item["origin"].as_point().unwrap_or(LayoutPoint::new(0.0, 0.0));
            let glyph_indices = item["glyphs"].as_vec_u32().unwrap();
            let glyph_offsets = item["offsets"].as_vec_f32().unwrap();
            assert!(glyph_offsets.len() == glyph_indices.len() * 2);

            let glyphs = glyph_indices.iter().enumerate().map(|k| {
                GlyphInstance {
                    index: *k.1,
                    x: origin.x + glyph_offsets[k.0*2],
                    y: origin.y + glyph_offsets[k.0*2+1],
                }
            }).collect();
            // TODO(gw): We could optionally use the WR API to query glyph dimensions
            //           here and calculate the bounding region here if we want to.
            let rect = item["bounds"].as_rect()
                                     .expect("Text items with glyphs require bounds [for now]");
            (glyphs, rect)
        } else {
            if native_key.is_none() {
                panic!("Can't layout simple ascii text with raw font [for now]");
            }
            let native_key = native_key.unwrap();
            let text = item["text"].as_str().unwrap();
            let (glyph_indices, glyph_advances) =
                layout_simple_ascii(native_key, text, size);
            let origin = item["origin"].as_point()
                .expect("origin required for text without glyphs");

            let mut x = origin.x;
            let y = origin.y;
            let glyphs = glyph_indices.iter().zip(glyph_advances).map(|arg| {
                let gi = GlyphInstance { index: *arg.0 as u32, x: x, y: y };
                x = x + arg.1;
                gi
            }).collect();
            let rect = LayoutRect::new(LayoutPoint::new(0.0, 0.0), wrench.window_size_f32());
            (glyphs, rect)
        };

        let builder = self.builder();
        let clip = item["clip"].as_clip_region(builder).unwrap_or(*clip_region);
        // FIXME this is the full bounds of the glyphs; we should calculate this more accurately
        builder.push_text(rect, clip, glyphs, font_key, color, size, blur_radius);
    }

    pub fn add_display_list_items_from_yaml(&mut self, wrench: &mut Wrench, yaml: &Yaml) {
        let full_clip_region = {
            let win_size = wrench.window_size_f32();
            self.builder().new_clip_region(&LayoutRect::new(LayoutPoint::new(0.0, 0.0), win_size),
                                           Vec::new(), None)
        };

        for ref item in yaml.as_vec().unwrap() {
            // handle shorthand first
            if !item["rect"].is_badvalue() {
                self.handle_rect(wrench, &full_clip_region, &item);
                continue;
            }

            if !item["image"].is_badvalue() {
                self.handle_image(wrench, &full_clip_region, &item);
                continue;
            }

            if !item["text"].is_badvalue() || !item["glyphs"].is_badvalue() {
                self.handle_text(wrench, &full_clip_region, &item);
                continue;
            }

            if !item["stacking_context"].is_badvalue() {
                self.add_stacking_context_from_yaml(wrench, &item);
                continue;
            }

            // handle 'type: xxx' longhand
            match item["type"].as_str() {
                Some("rect") => self.handle_rect(wrench, &full_clip_region, &item),
                Some("image") => self.handle_image(wrench, &full_clip_region, &item),
                Some("text") => self.handle_text(wrench, &full_clip_region, &item),
                Some("stacking_context") => self.add_stacking_context_from_yaml(wrench, &item),
                _ => {
                    //println!("Skipping {:?}", item);
                }
            }
        }
    }

    pub fn add_stacking_context_from_yaml(&mut self, wrench: &mut Wrench, yaml: &Yaml) {
        let bounds = yaml["bounds"].as_rect().unwrap_or(LayoutRect::new(LayoutPoint::new(0.0, 0.0), wrench.window_size_f32()));
        let overflow_bounds = yaml["overflow"].as_rect().unwrap_or(bounds);
        let z_index = yaml["z_index"].as_i64().unwrap_or(0);
        let transform = yaml["transform"].as_matrix4d().unwrap_or(LayoutTransform::identity());
        let perspective = yaml["perspective"].as_matrix4d().unwrap_or(LayoutTransform::identity());

        // FIXME handle these
        let mix_blend_mode = MixBlendMode::Normal;
        let filters: Vec<FilterOp> = Vec::new();

        {
            let builder = self.builder();
            let clip = builder.new_clip_region(&overflow_bounds, vec![], None);
            builder.push_stacking_context(ScrollPolicy::Scrollable,
                                          bounds,
                                          clip,
                                          z_index as i32,
                                          &transform,
                                          &perspective,
                                          mix_blend_mode,
                                          filters);
        }

        if !yaml["items"].is_badvalue() {
            self.add_display_list_items_from_yaml(wrench, &yaml["items"]);
        }

        self.builder().pop_stacking_context();
    }
}

impl WrenchThing for YamlFrameReader {
    fn do_frame(&mut self, wrench: &mut Wrench) -> u32 {
        if !self.frame_built {
            self.builder = Some(DisplayListBuilder::new(wrench.root_pipeline_id));

            self.build(wrench);
        }

        self.frame_count += 1;

        if !self.frame_built || wrench.should_rebuild_display_lists() {
            wrench.send_lists(self.frame_count, self.builder.as_ref().unwrap().clone());
        } else {
            wrench.refresh();
        }

        self.frame_built = true;
        self.frame_count
    }

    fn next_frame(&mut self) {
    }

    fn prev_frame(&mut self) {
    }

    fn queue_frames(&self) -> u32 {
        self.queue_depth
    }
}
