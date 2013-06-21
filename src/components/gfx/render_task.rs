/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// The task that handles all rendering/painting.

use azure::{AzFloat, AzGLContext};
use azure::azure_hl::{B8G8R8A8, DrawTarget};
use display_list::DisplayList;
use servo_msg::compositor::{RenderListener, IdleRenderState, RenderingRenderState, LayerBuffer};
use servo_msg::compositor::{CompositorToken, LayerBufferSet};
use servo_msg::engine::{EngineChan, TokenSurrenderMsg};
use font_context::FontContext;
use geom::matrix2d::Matrix2D;
use geom::point::Point2D;
use geom::size::Size2D;
use geom::rect::Rect;
use opts::Opts;
use render_context::RenderContext;

use core::cell::Cell;
use core::comm::{Chan, Port, SharedChan};
use core::util::replace;

use servo_util::time::{ProfilerChan, profile};
use servo_util::time;

pub struct RenderLayer {
    display_list: DisplayList<()>,
    size: Size2D<uint>
}

pub enum Msg {
    RenderMsg(RenderLayer),
    TokenBestowMsg(~CompositorToken),
    TokenProcureMsg,
    ExitMsg(Chan<()>),
}

#[deriving(Clone)]
pub struct RenderChan {
    chan: SharedChan<Msg>,
}

impl RenderChan {
    pub fn new(chan: Chan<Msg>) -> RenderChan {
        RenderChan {
            chan: SharedChan::new(chan),
        }
    }
    pub fn send(&self, msg: Msg) {
        self.chan.send(msg);
    }
}

priv struct RenderTask<C> {
    port: Port<Msg>,
    compositor: C,
    font_ctx: @mut FontContext,
    opts: Opts,

    /// A channel to the profiler.
    profiler_chan: ProfilerChan,

    share_gl_context: AzGLContext,

    /// A channel to the engine task for surrendering token
    engine_chan: EngineChan,
    /// A token that grants permission to send paint messages to compositor
    compositor_token: Option<~CompositorToken>,
    /// Cached copy of last layers rendered
    next_paint_msg: Option<(LayerBufferSet, Size2D<uint>)>,
}

impl<C: RenderListener + Owned> RenderTask<C> {
    pub fn create(port: Port<Msg>,
                  compositor: C,
                  opts: Opts,
                  engine_chan: EngineChan,
                  profiler_chan: ProfilerChan) {
        let compositor_cell = Cell(compositor);
        let opts_cell = Cell(opts);
        let port = Cell(port);
        let engine_chan = Cell(engine_chan);

        do spawn {
            let compositor = compositor_cell.take();
            let share_gl_context = compositor.get_gl_context();
            let opts = opts_cell.with_ref(|o| copy *o);
            let profiler_chan = profiler_chan.clone();
            let profiler_chan_clone = profiler_chan.clone();

            // FIXME: rust/#5967
            let mut render_task = RenderTask {
                port: port.take(),
                compositor: compositor,
                font_ctx: @mut FontContext::new(opts.render_backend,
                                                false,
                                                profiler_chan),
                opts: opts_cell.take(),
                profiler_chan: profiler_chan_clone,
                share_gl_context: share_gl_context,

                engine_chan: engine_chan.take(),
                compositor_token: None,
                next_paint_msg: None,
            };

            render_task.start();
        }
    }

    fn start(&mut self) {
        debug!("render_task: beginning rendering loop");

        loop {
            match self.port.recv() {
                RenderMsg(render_layer) => self.render(render_layer),
                TokenBestowMsg(token) => {
                    self.compositor_token = Some(token);
                    let next_paint_msg = replace(&mut self.next_paint_msg, None);
                    match next_paint_msg {
                        Some((layer_buffer_set, layer_size)) => {
                            println("retrieving cached paint msg");
                            self.compositor.paint(layer_buffer_set, layer_size);
                            self.compositor.set_render_state(IdleRenderState);
                        }
                        None => {}
                    }
                }
                TokenProcureMsg => {
                    self.engine_chan.send(TokenSurrenderMsg(self.compositor_token.swap_unwrap()));
                }
                ExitMsg(response_ch) => {
                    response_ch.send(());
                    break;
                }
            }
        }
    }

    fn render(&mut self, render_layer: RenderLayer) {
        debug!("render_task: rendering");
        self.compositor.set_render_state(RenderingRenderState);
        do profile(time::RenderingCategory, self.profiler_chan.clone()) {
            let tile_size = self.opts.tile_size;
            let scale = self.opts.zoom;

            // FIXME: Try not to create a new array here.
            let mut new_buffers = ~[];

            // Divide up the layer into tiles.
            do time::profile(time::RenderingPrepBuffCategory, self.profiler_chan.clone()) {
                let mut y = 0;
                while y < render_layer.size.height * scale {
                    let mut x = 0;
                    while x < render_layer.size.width * scale {
                        // Figure out the dimension of this tile.
                        let right = uint::min(x + tile_size, render_layer.size.width * scale);
                        let bottom = uint::min(y + tile_size, render_layer.size.height * scale);
                        let width = right - x;
                        let height = bottom - y;

                        let tile_rect = Rect(Point2D(x / scale, y / scale),
                                             Size2D(width, height));   //change this
                        let screen_rect = Rect(Point2D(x, y),
                                               Size2D(width, height)); //change this

                        let buffer = LayerBuffer {
                            draw_target: DrawTarget::new_with_fbo(self.opts.render_backend,
                                                                  self.share_gl_context,
                                                                  Size2D(width as i32,
                                                                         height as i32),
                                                                  B8G8R8A8),
                            rect: tile_rect,
                            screen_pos: screen_rect,
                            stride: (width * 4) as uint
                        };

                        {
                            // Build the render context.
                            let ctx = RenderContext {
                                canvas: &buffer,
                                font_ctx: self.font_ctx,
                                opts: &self.opts
                            };

                            // Apply the translation to render the tile we want.
                            let matrix: Matrix2D<AzFloat> = Matrix2D::identity();
                            let matrix = matrix.scale(scale as AzFloat, scale as AzFloat);
                            let matrix = matrix.translate(-(buffer.rect.origin.x as f32) as AzFloat,
                                                          -(buffer.rect.origin.y as f32) as AzFloat);

                            ctx.canvas.draw_target.set_transform(&matrix);

                            // Clear the buffer.
                            ctx.clear();

                            // Draw the display list.
                            do profile(time::RenderingDrawingCategory, self.profiler_chan.clone()) {
                                render_layer.display_list.draw_into_context(&ctx);
                                ctx.canvas.draw_target.flush();
                            }
                        }

                        new_buffers.push(buffer);

                        x += tile_size;
                    }

                    y += tile_size;
                }
            }

            let layer_buffer_set = LayerBufferSet {
                buffers: new_buffers,
            };

            debug!("render_task: returning surface");
            if self.compositor_token.is_some() {
                self.compositor.paint(layer_buffer_set, render_layer.size);
            }
            else {
                println("caching paint msg");
                self.next_paint_msg = Some((layer_buffer_set, render_layer.size));
            }
            self.compositor.set_render_state(IdleRenderState);
        }
    }
}

