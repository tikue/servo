/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use compositing::{CompositorChan, SetLayoutChan};
use core::cell::Cell;
use core::comm::Port;
use gfx::opts::Opts;
use gfx::render_task::{RenderChan, TokenBestowMsg, TokenProcureMsg};
use pipeline::Pipeline;
use servo_msg::compositor::{CompositorToken};
use servo_msg::engine::{EngineChan, ExitMsg, LoadUrlMsg, Msg, RendererReadyMsg, TokenSurrenderMsg};
use script::script_task::{ExecuteMsg, LoadMsg};
use servo_net::image_cache_task::{ImageCacheTask, ImageCacheTaskClient};
use servo_net::resource_task::ResourceTask;
use servo_net::resource_task;
use servo_util::time::ProfilerChan;
use core::hashmap::HashMap;

pub struct Engine {
    chan: EngineChan,
    request_port: Port<Msg>,
    compositor_chan: CompositorChan,
    resource_task: ResourceTask,
    image_cache_task: ImageCacheTask,
    pipelines: HashMap<str, Pipeline>,
    next_id: uint,
    current_token_holder: Option<RenderChan>,
    next_token_holder: Option<uint>,
    profiler_chan: ProfilerChan,
    opts: Opts,
}

pub struct Pipeline {
    script_chan: ScriptChan,
    presentations: HashMap<uint, Presentation>,
}

impl Pipeline {
    pub fn create(compositor_chan: CompositorChan,
                  resource_task: ResourceTask,
                  image_cache_task: ImageCacheTask) -> Pipeline {

        let (script_port, script_chan) = comm::stream();
        let script_chan = ScriptChan::new(script_chan);
        ScriptTask::create(compositor_chan.clone(),
                           script_port,
                           script_chan.clone(),
                           resource_task.clone(),
                           image_cache_task.clone());
        Pipeline::new(script_chan)
    }

    pub fn new(script_chan) -> Pipeline {
        Pipeline {
            script_chan: script_chan,
            presentations: HashMap::new(),
        }
    }

    pub fn exit(&self) {
        self.script_chan.send(ExitMsg);
        for self.presentations.each |_, presentation| {
            presentation.exit();
        }
    }
}

impl Engine {
    pub fn start(compositor_chan: CompositorChan,
                 opts: &Opts,
                 resource_task: ResourceTask,
                 image_cache_task: ImageCacheTask,
                 profiler_chan: ProfilerChan)
                 -> EngineChan {
            
        // Create the engine port and channel.
        let (engine_port, engine_chan) = comm::stream();
        let (engine_port, engine_chan) = (Cell(engine_port), EngineChan::new(engine_chan));

        let compositor_chan = Cell(compositor_chan);

        let opts = Cell(copy *opts);
        let engine_chan_clone = Cell(engine_chan.clone());
        {
            do task::spawn {
                let mut engine = Engine {
                    chan: engine_chan_clone.take(),
                    request_port: engine_port.take(),
                    compositor_chan: compositor_chan.take(),
                    resource_task: resource_task.clone(),
                    image_cache_task: image_cache_task.clone(),
                    pipelines: HashMap::new(),
                    next_id: 0,
                    current_token_holder: None,
                    next_token_holder: None,
                    profiler_chan: profiler_chan.clone(),
                    opts: opts.take(),
                };
                engine.run();
            }
        }
        engine_chan
    }

    fn run(&mut self) {
        loop {
            let request = self.request_port.recv();
            if !self.handle_request(request) {
                break;
            }
        }
    }

    fn get_next_id(&mut self) -> uint {
        let id = self.next_id;
        self.next_id = id + 1;
        id
    }

    fn handle_request(&mut self, request: Msg) -> bool {
        match request {
            LoadUrlMsg(url) => {
                let pipeline = match self.pipelines.find(url.host) {
                    None => {
                        let pipeline = Pipeline::create(self.compositor_chan.clone(),
                                         self.resource_chan.clone(),
                                         self.image_cache_task.clone());
                        self.pipelines.insert(url.host, pipeline);
                        pipeline
                    }
                    Some(pipeline) => pipeline
                };
                
                if url.path.ends_with(".js") {
                    pipeline.script_chan.send(ExecuteMsg(url));
                } else {
                    let presentation_id = self.get_next_id();
                    let presentation = Presentation::create(presentation_id,
                                                            self.chan.clone(),
                                                            pipeline.script_chan.clone(),
                                                            self.compositor_chan.clone(),
                                                            self.image_cache_task.clone(),
                                                            self.resource_task.clone(),
                                                            self.profiler_chan.clone(),
                                                            copy self.opts);
                    pipeline.script_chan.send(LoadMsg(presentation_id,
                                                      presentation.layout_chan.clone(),
                                                      url));
                    self.next_token_holder = Some(pipeline_id);
                }
                self.pipelines.insert(pipeline_id, pipeline);
            }

            RendererReadyMsg(pipeline_id) => {
                let next_token_holder = self.next_token_holder.clone();
                do next_token_holder.map() |id| {
                    if pipeline_id == *id {
                        match self.current_token_holder {
                            Some(ref id) => {
                                let current_holder = self.pipelines.find(id).get();
                                current_holder.send(TokenProcureMsg);
                            }
                            None => self.bestow_compositor_token(id, ~CompositorToken::new())
                        }
                    }
                };
            }

            TokenSurrenderMsg(token) => {
                self.remove_active_pipeline();
                let next_token_holder = self.next_token_holder.clone();
                let token = Cell(token);
                do next_token_holder.map |id| {
                    self.bestow_compositor_token(id, token.take());
                };
            }

            ExitMsg(sender) => {
                for self.pipelines.each |_, pipeline| {
                    pipeline.exit();
                }
                self.image_cache_task.exit();
                self.resource_task.send(resource_task::Exit);

                sender.send(());
                return false
            }
        }
        true
    }
    
    fn remove_active_pipeline(&mut self) {
// FIXME(tkuehn): currently, pipelines are not removed at all
//        do self.current_token_holder.map |id| {
//            self.pipelines.pop(id).unwrap().exit();
//        };

        self.current_token_holder = None;
    }

    fn bestow_compositor_token(&mut self, id: &uint, compositor_token: ~CompositorToken) {
        let pipeline = self.pipelines.find(id);
        match pipeline {
            None => fail!("Id of pipeline that made token request does not have a \
                          corresponding struct in Engine's pipelines. This is a bug. :-("),
            Some(pipeline) => {
                let render_chan = pipeline.presentation.render_chan.clone();
                render_chan.send(TokenBestowMsg(compositor_token));
                self.compositor_chan.send(SetLayoutChan(pipeline.presentation.layout_chan.clone()));
                self.current_token_holder = Some(render_chan);
                self.next_token_holder = None;
            }
        }
    }
}

