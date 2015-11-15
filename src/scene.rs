use fnv::FnvHasher;
use internal_types::DrawListId;
use optimizer;
use resource_cache::ResourceCache;
use std::collections::HashMap;
use std::collections::hash_state::DefaultState;
use webrender_traits::{PipelineId, Epoch};
use webrender_traits::{DisplayListBuilder};
use webrender_traits::{ColorF, DisplayListId, StackingContext, StackingContextId};
use webrender_traits::{SpecificDisplayListItem};
use webrender_traits::{StackingLevel, SpecificDisplayItem};
use webrender_traits::{IframeInfo};
use webrender_traits::{RectangleDisplayItem, ClipRegion, DisplayItem};

#[derive(Debug)]
pub struct ScenePipeline {
    pub pipeline_id: PipelineId,
    pub epoch: Epoch,
    pub background_draw_list: Option<DrawListId>,
    pub root_stacking_context_id: StackingContextId,
}

pub struct Scene {
    pub root_pipeline_id: Option<PipelineId>,
    pub pipeline_map: HashMap<PipelineId, ScenePipeline, DefaultState<FnvHasher>>,
    pub display_list_map: HashMap<DisplayListId, SceneDisplayList, DefaultState<FnvHasher>>,
    pub stacking_context_map: HashMap<StackingContextId, SceneStackingContext, DefaultState<FnvHasher>>,
}

#[derive(Clone, Debug)]
pub enum SpecificSceneItem {
    DrawList(DrawListId),
    StackingContext(StackingContextId),
    Iframe(Box<IframeInfo>),
}

#[derive(Clone, Debug)]
pub struct SceneItem {
    pub stacking_level: StackingLevel,
    pub specific: SpecificSceneItem,
}

pub struct SceneDisplayList {
    pub pipeline_id: PipelineId,
    pub epoch: Epoch,
    pub items: Vec<SceneItem>,
}

pub struct SceneStackingContext {
    pub pipeline_id: PipelineId,
    pub epoch: Epoch,
    pub stacking_context: StackingContext,
}

impl Scene {
    pub fn new() -> Scene {
        Scene {
            root_pipeline_id: None,
            pipeline_map: HashMap::with_hash_state(Default::default()),
            display_list_map: HashMap::with_hash_state(Default::default()),
            stacking_context_map: HashMap::with_hash_state(Default::default()),
        }
    }

    pub fn add_display_list(&mut self,
                        id: DisplayListId,
                        pipeline_id: PipelineId,
                        epoch: Epoch,
                        mut display_list_builder: DisplayListBuilder,
                        resource_cache: &mut ResourceCache) {
        display_list_builder.finalize();
        optimizer::optimize_display_list_builder(&mut display_list_builder);

        let items = display_list_builder.items.into_iter().map(|item| {
            match item.specific {
                SpecificDisplayListItem::DrawList(info) => {
                    let draw_list_id = resource_cache.add_draw_list(info.items);
                    SceneItem {
                        stacking_level: item.stacking_level,
                        specific: SpecificSceneItem::DrawList(draw_list_id)
                    }
                }
                SpecificDisplayListItem::StackingContext(info) => {
                    SceneItem {
                        stacking_level: item.stacking_level,
                        specific: SpecificSceneItem::StackingContext(info.id)
                    }
                }
                SpecificDisplayListItem::Iframe(info) => {
                    SceneItem {
                        stacking_level: item.stacking_level,
                        specific: SpecificSceneItem::Iframe(info)
                    }
                }
            }
        }).collect();

        let display_list = SceneDisplayList {
            pipeline_id: pipeline_id,
            epoch: epoch,
            items: items,
        };

        self.display_list_map.insert(id, display_list);
    }

    pub fn add_stacking_context(&mut self,
                            id: StackingContextId,
                            pipeline_id: PipelineId,
                            epoch: Epoch,
                            stacking_context: StackingContext) {
        let stacking_context = SceneStackingContext {
            pipeline_id: pipeline_id,
            epoch: epoch,
            stacking_context: stacking_context,
        };

        self.stacking_context_map.insert(id, stacking_context);
    }

    pub fn set_root_pipeline_id(&mut self, pipeline_id: PipelineId) {
        self.root_pipeline_id = Some(pipeline_id);
    }

    pub fn set_root_stacking_context(&mut self,
                                 pipeline_id: PipelineId,
                                 epoch: Epoch,
                                 stacking_context_id: StackingContextId,
                                 background_color: ColorF,
                                 resource_cache: &mut ResourceCache) {
        let old_display_list_keys: Vec<_> = self.display_list_map.iter()
                                                .filter(|&(_, ref v)| {
                                                    v.pipeline_id == pipeline_id &&
                                                    v.epoch < epoch
                                                })
                                                .map(|(k, _)| k.clone())
                                                .collect();

        // Remove any old draw lists and display lists for this pipeline
        for key in old_display_list_keys {
            let display_list = self.display_list_map.remove(&key).unwrap();
            for item in display_list.items {
                match item.specific {
                    SpecificSceneItem::DrawList(draw_list_id) => {
                        resource_cache.remove_draw_list(draw_list_id);
                    }
                    SpecificSceneItem::StackingContext(..) |
                    SpecificSceneItem::Iframe(..) => {}
                }
            }
        }

        let old_stacking_context_keys: Vec<_> = self.stacking_context_map.iter()
                                                                         .filter(|&(_, ref v)| {
                                                                             v.pipeline_id == pipeline_id &&
                                                                             v.epoch < epoch
                                                                         })
                                                                         .map(|(k, _)| k.clone())
                                                                         .collect();

        // Remove any old draw lists and display lists for this pipeline
        for key in old_stacking_context_keys {
            self.stacking_context_map.remove(&key).unwrap();

            // TODO: Could remove all associated DLs here,
            //       and then the above code could just be a debug assert check...
        }

        let background_draw_list = if background_color.a > 0.0 {
            let overflow = self.stacking_context_map[&stacking_context_id].stacking_context.overflow;

            let rectangle_item = RectangleDisplayItem {
                color: background_color,
            };
            let clip = ClipRegion {
                main: overflow,
                complex: vec![],
            };
            let root_bg_color_item = DisplayItem {
                item: SpecificDisplayItem::Rectangle(rectangle_item),
                rect: overflow,
                clip: clip,
            };

            let draw_list_id = resource_cache.add_draw_list(vec![root_bg_color_item]);
            Some(draw_list_id)
        } else {
            None
        };

        let new_pipeline = ScenePipeline {
            pipeline_id: pipeline_id,
            epoch: epoch,
            background_draw_list: background_draw_list,
            root_stacking_context_id: stacking_context_id,
        };

        if let Some(old_pipeline) = self.pipeline_map.insert(pipeline_id, new_pipeline) {
            if let Some(background_draw_list) = old_pipeline.background_draw_list {
                resource_cache.remove_draw_list(background_draw_list);
            }
        }
    }
}