use basedrop::{Collector, Handle, Shared, SharedCell};
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LockResult, Mutex,
    },
};

use fnv::FnvHashMap;
use rusty_daw_time::{MusicalTime, SampleRate, SampleTime, Seconds, TempoMap};

use crate::backend::graph_interface::{CompiledGraph, GraphInterface, NodeID, PortType};
use crate::backend::resource_loader::{ResourceLoadError, ResourceLoader};
use crate::backend::timeline::{
    LoopState, TimelineTrackHandle, TimelineTrackSaveState, TimelineTransportHandle,
    TimelineTransportSaveState,
};
use crate::backend::{generic_nodes, timeline::AudioClipSaveState};

use super::timeline::TimelineTrackNode;

static COLLECT_INTERVAL: Duration = Duration::from_secs(3);

static DEFAULT_AUDIO_CLIP_DECLICK_TIME: Seconds = Seconds(10.0 / 1_000.0);

/// This struct should contain all information needed to create a "save file"
/// for the project.
///
/// TODO: Project file format. This will need to be future-proof.
pub struct ProjectSaveState {
    pub timeline_tracks: Vec<TimelineTrackSaveState>,
    pub timeline_transport: TimelineTransportSaveState,
    pub tempo_map: TempoMap,
    pub audio_clip_declick_time: Seconds,
}

impl ProjectSaveState {
    pub fn new_empty(sample_rate: SampleRate) -> Self {
        Self {
            timeline_tracks: Vec::new(),
            timeline_transport: Default::default(),
            tempo_map: TempoMap::new(110.0, sample_rate.into()),
            audio_clip_declick_time: DEFAULT_AUDIO_CLIP_DECLICK_TIME,
        }
    }

    pub fn test(sample_rate: SampleRate) -> Self {
        let mut new_self = ProjectSaveState::new_empty(sample_rate);

        new_self.timeline_transport.loop_state = LoopState::Active {
            loop_start: SampleTime::new(0),
            loop_end: SampleTime::new(50_000),
        };

        new_self.timeline_tracks.push(TimelineTrackSaveState {
            id: String::from("Track 1"),
            audio_clips: vec![
                AudioClipSaveState {
                    id: String::from("Audio Clip 1"),
                    pcm_path: "./test_files/synth_keys/synth_keys_44100_16bit.wav".into(),
                    timeline_start: MusicalTime::new(0.0),
                    duration: Seconds::new(10.0),
                    clip_start_offset: Seconds::new(0.0),
                    clip_gain_db: -6.0,
                },
                AudioClipSaveState {
                    id: String::from("Audio Clip 2"),
                    pcm_path: "./test_files/synth_keys/synth_keys_44100_16bit.wav".into(),
                    timeline_start: MusicalTime::new(1.0),
                    duration: Seconds::new(10.0),
                    clip_start_offset: Seconds::new(0.0),
                    clip_gain_db: -6.0,
                },
            ],
        });

        new_self
    }
}

/// All operations that affect the project state must happen through one of this struct's
/// methods. As such this struct just be responsible for checking that the project state
/// always remains valid. This will also allow us to create a scripting api later on.
pub struct ProjectInterface {
    save_state: ProjectSaveState,

    graph_interface: GraphInterface,

    resource_loader: Arc<Mutex<ResourceLoader>>,

    timeline_track_indexes: FnvHashMap<String, usize>,
    timeline_track_handles: Vec<TimelineTrackHandle>,
    timeline_track_node_ids: Vec<NodeID>,

    timeline_transport: TimelineTransportHandle,

    master_track_mix_in_node_id: NodeID,

    sample_rate: SampleRate,

    coll_handle: Handle,

    running: Arc<AtomicBool>,
}

impl ProjectInterface {
    pub fn new(
        save_state: ProjectSaveState,
        sample_rate: SampleRate,
    ) -> (
        Self,
        Shared<SharedCell<CompiledGraph>>,
        Vec<ResourceLoadError>,
    ) {
        let collector = Collector::new();
        let coll_handle = collector.handle();

        let resource_loader = Arc::new(Mutex::new(ResourceLoader::new(
            collector.handle(),
            sample_rate,
        )));
        let resource_loader_clone = Arc::clone(&resource_loader);

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        std::thread::spawn(|| run_collector(collector, resource_loader_clone, running_clone));

        let mut load_errors = Vec::<ResourceLoadError>::new();
        let mut timeline_track_indexes = FnvHashMap::<String, usize>::default();
        let mut timeline_track_handles = Vec::<TimelineTrackHandle>::new();
        let mut timeline_track_node_ids = Vec::<NodeID>::new();

        let (mut graph_interface, rt_graph_interface, timeline_transport) =
            GraphInterface::new(sample_rate, coll_handle.clone(), &&save_state);

        let mut master_track_mix_in_node_id = None;

        graph_interface.modify_graph(|mut graph| {
            for (timeline_track_index, timeline_track_save) in
                save_state.timeline_tracks.iter().enumerate()
            {
                let (node, handle, mut res) = TimelineTrackNode::new(
                    timeline_track_save,
                    &resource_loader,
                    &save_state.tempo_map,
                    sample_rate,
                    coll_handle.clone(),
                );

                // Append any errors that happened while loading resources.
                load_errors.append(&mut res);

                let node_id = graph.add_new_node(Box::new(node));

                timeline_track_indexes.insert(timeline_track_save.id.clone(), timeline_track_index);
                timeline_track_handles.push(handle);
                timeline_track_node_ids.push(node_id);
            }

            // All timeline tracks will be mixed into a single "master" track.
            //
            // TODO: Track routing.
            let master_track_mix_id = graph.add_new_node(Box::new(
                generic_nodes::mix::StereoMixNode::new(timeline_track_handles.len()),
            ));

            // Connect all timeline tracks to the "master" track.
            //
            // TODO: Track routing.
            for (i, node_id) in timeline_track_node_ids.iter().enumerate() {
                graph
                    .add_port_connection(PortType::StereoAudio, node_id, 0, &master_track_mix_id, i)
                    .unwrap();
            }

            master_track_mix_in_node_id = Some(master_track_mix_id);
        });

        (
            Self {
                save_state,

                graph_interface,
                resource_loader,

                timeline_track_indexes,
                timeline_track_handles,
                timeline_track_node_ids,

                timeline_transport,

                master_track_mix_in_node_id: master_track_mix_in_node_id.unwrap(),

                sample_rate,
                coll_handle,

                running,
            },
            rt_graph_interface,
            load_errors,
        )
    }

    /// Return an immutable handle to the timeline track with given ID.
    pub fn timeline_track<'a>(
        &'a self,
        id: &String,
    ) -> Option<(&'a TimelineTrackHandle, &'a TimelineTrackSaveState)> {
        if let Some(index) = self.timeline_track_indexes.get(id) {
            Some((
                &self.timeline_track_handles[*index],
                &self.save_state.timeline_tracks[*index],
            ))
        } else {
            None
        }
    }

    /// Return a mutable handle to the timeline track with given ID.
    pub fn timeline_track_mut<'a>(
        &'a mut self,
        id: &String,
    ) -> Option<(
        &'a mut TimelineTrackHandle,
        &'a mut TimelineTrackSaveState,
        &'a Arc<Mutex<ResourceLoader>>,
    )> {
        if let Some(index) = self.timeline_track_indexes.get(id) {
            Some((
                &mut self.timeline_track_handles[*index],
                &mut self.save_state.timeline_tracks[*index],
                &mut self.resource_loader,
            ))
        } else {
            None
        }
    }

    /// Set the ID of the timeline track. The timeline track's ID is used as the name. It must be unique for this project.
    ///
    /// TODO: Return custom error.
    pub fn set_timeline_track_id(&mut self, old_id: &String, new_id: String) -> Result<(), ()> {
        if self.timeline_track_indexes.contains_key(&new_id) {
            return Err(());
        }

        if let Some(index) = self.timeline_track_indexes.remove(old_id) {
            self.timeline_track_indexes.insert(new_id.clone(), index);

            // Update the values in the save state.
            self.save_state.timeline_tracks[index].id = new_id;

            // TODO: Alert the GUI of the change.

            Ok(())
        } else {
            Err(())
        }
    }

    pub fn add_timeline_track(
        &mut self,
        track: TimelineTrackSaveState,
    ) -> Result<Vec<ResourceLoadError>, ()> {
        if self.timeline_track_indexes.contains_key(&track.id) {
            return Err(());
        }

        let mut load_errors = Vec::<ResourceLoadError>::new();

        let timeline_track_index = self.save_state.timeline_tracks.len();
        self.timeline_track_indexes
            .insert(track.id.clone(), timeline_track_index);

        let (node, handle, mut res) = TimelineTrackNode::new(
            &track,
            &self.resource_loader,
            &self.save_state.tempo_map,
            self.sample_rate,
            self.coll_handle.clone(),
        );

        // Append any errors that happened while loading resources.
        load_errors.append(&mut res);

        self.timeline_track_indexes
            .insert(track.id.clone(), timeline_track_index);
        self.timeline_track_handles.push(handle);

        self.save_state.timeline_tracks.push(track);

        let mut node_id = None;
        let num_timeline_tracks = self.save_state.timeline_tracks.len();
        let master_track_mix_in_node_id = self.master_track_mix_in_node_id;

        self.graph_interface.modify_graph(|mut graph| {
            let n_id = graph.add_new_node(Box::new(node));

            // All timeline tracks will be mixed into a single "master" track.
            //
            // TODO: Track routing.
            //
            // Replace the current mix node with one that has the correct number of inputs.
            let master_mix_node = generic_nodes::mix::StereoMixNode::new(num_timeline_tracks);
            graph
                .replace_node(&master_track_mix_in_node_id, Box::new(master_mix_node))
                .unwrap();

            // Connect the new track to the "master" track;
            graph
                .add_port_connection(
                    PortType::StereoAudio,
                    &n_id,
                    0,
                    &master_track_mix_in_node_id,
                    num_timeline_tracks - 1,
                )
                .unwrap();

            node_id = Some(n_id);
        });

        self.timeline_track_node_ids.push(node_id.unwrap());

        Ok(load_errors)
    }

    pub fn remove_timeline_track(&mut self, id: &String) -> Result<(), ()> {
        if let Some(index) = self.timeline_track_indexes.remove(id) {
            self.save_state.timeline_tracks.remove(index);
            self.timeline_track_handles.remove(index);

            let node_id = self.timeline_track_node_ids.remove(index);

            self.graph_interface.modify_graph(|mut graph| {
                graph.remove_node(&node_id).unwrap();
            });

            Ok(())
        } else {
            Err(())
        }
    }

    pub fn timeline_transport_mut(&mut self) -> &mut TimelineTransportHandle {
        &mut self.timeline_transport
    }
}

impl Drop for ProjectInterface {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

fn run_collector(
    mut collector: Collector,
    resource_loader: Arc<Mutex<ResourceLoader>>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::SeqCst) {
        std::thread::sleep(COLLECT_INTERVAL);

        {
            match resource_loader.lock() {
                LockResult::Ok(mut res_loader) => {
                    res_loader.collect();
                }
                LockResult::Err(e) => {
                    log::error!("{}", e);
                    break;
                }
            }
        }

        collector.collect();
    }
    log::info!("shutting down collector");
}
