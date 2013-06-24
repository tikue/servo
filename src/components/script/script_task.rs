/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

/// The script task is the task that owns the DOM in memory, runs JavaScript, and spawns parsing
/// and layout tasks.

use servo_msg::compositor::{ScriptListener, Loading, PerformingLayout};
use servo_msg::compositor::FinishedLoading;
use dom::bindings::utils::GlobalStaticData;
use dom::document::Document;
use dom::element::Element;
use dom::event::{Event, ResizeEvent, ReflowEvent, ClickEvent, MouseDownEvent, MouseUpEvent};
use dom::node::{AbstractNode, ScriptView, define_bindings};
use dom::window::Window;
use layout_interface::{AddStylesheetMsg, DocumentDamage, LayoutChan};
use layout_interface::{DocumentDamageLevel, HitTestQuery, HitTestResponse, LayoutQuery};
use layout_interface::{MatchSelectorsDocumentDamage, QueryMsg, Reflow};
use layout_interface::{ReflowDocumentDamage, ReflowForDisplay, ReflowForScriptQuery, ReflowGoal};
use layout_interface::ReflowMsg;
use layout_interface;
use servo_msg::engine::{EngineChan, LoadUrlMsg};

use core::cast::transmute;
use core::cell::Cell;
use core::comm::{Port, SharedChan};
use core::io::read_whole_file;
use core::local_data;
use core::ptr::null;
use core::task::{SingleThreaded, task};
use core::util::replace;
use dom::window::TimerData;
use geom::size::Size2D;
use html::hubbub_html_parser;
use js::JSVAL_NULL;
use js::global::{global_class, debug_fns};
use js::glue::bindgen::RUST_JSVAL_TO_OBJECT;
use js::jsapi::JSContext;
use js::jsapi::bindgen::{JS_CallFunctionValue, JS_GetContextPrivate};
use js::rust::{Compartment, Cx};
use js;
use servo_net::image_cache_task::ImageCacheTask;
use servo_net::resource_task::ResourceTask;
use servo_util::tree::TreeNodeRef;
use servo_util::url::make_url;
use std::net::url::Url;
use std::net::url;
use core::hashmap::HashMap;

/// Messages used to control the script task.
pub enum ScriptMsg {
    /// Sends a Url to be loaded, along with a handle to the responsible layout
    LoadMsg(uint, LayoutChan, Url),
    /// Executes a standalone script.
    ExecuteMsg(Url),
    /// Sends a DOM event, along with the active presentation id
    SendEventMsg(uint, Event),
    /// Fires a JavaScript timeout.
    FireTimerMsg(~TimerData),
    /// Notifies script that reflow is finished.
    ReflowCompleteMsg(uint),
    /// Exits the engine.
    ExitMsg,
}

/// Encapsulates external communication with the script task.
#[deriving(Clone)]
pub struct ScriptChan {
    /// The channel used to send messages to the script task.
    chan: SharedChan<ScriptMsg>,
}

impl ScriptChan {
    /// Creates a new script task.
    pub fn new(chan: Chan<ScriptMsg>) -> ScriptChan {
        ScriptChan {
            chan: SharedChan::new(chan)
        }
    }
    pub fn send(&self, msg: ScriptMsg) {
        self.chan.send(msg);
    }
}

/// Information for one frame in the browsing context.
pub struct Frame {
    document: @mut Document,
    window: @mut Window,
    url: Url,
}

/// Encapsulates a handle to a layout and its associated frame
pub struct LayoutInfo {
    chan: LayoutChan,
    /// The outermost frame. This frame contains the document, window, and page URL.
    root_frame: Frame,
    dom_static: GlobalStaticData,

    /// The port that we will use to join layout. If this is `None`, then layout is not currently
    /// running.
    layout_join_port: Option<Port<()>>,

    /// What parts of the document are dirty, if any.
    damage: Option<DocumentDamage>,
}

/// Information for an entire page. Pages are top-level browsing contexts and can contain multiple
/// frames.
///
/// FIXME: Rename to `Page`, following WebKit?
pub struct ScriptTask {
    /// A handle to all the layouts and their frames via their associated id's
    layout_map: HashMap<uint, LayoutInfo>,
    /// A handle to the image cache task.
    image_cache_task: ImageCacheTask,
    /// A handle to the resource task.
    resource_task: ResourceTask,

    /// The port on which we receive messages (load URL, exit, etc.)
    port: Port<ScriptMsg>,
    /// A channel for us to hand out when we want some other task to be able to send us script
    /// messages.
    chan: ScriptChan,

    /// For communicating load url messages to the engine
    engine_chan: EngineChan,
    /// For communicating ready state messages to the compositor
    compositor: @ScriptListener,

    /// The JavaScript runtime.
    js_runtime: js::rust::rt,
    /// The JavaScript context.
    js_context: @Cx,
    /// The JavaScript compartment.
    js_compartment: @mut Compartment,
    /// Global static data related to the DOM.
    dom_static: GlobalStaticData,
    /// Whether the JS bindings have been initialized.
    bindings_initialized: bool,

    /// The current size of the window, in pixels.
    window_size: Size2D<uint>,

}

fn global_script_context_key(_: @ScriptTask) {}

/// Returns this task's script context singleton.
pub fn global_script_context() -> @ScriptTask {
    unsafe {
        local_data::local_data_get(global_script_context_key).get()
    }
}

/// Returns the script task from the JS Context.
///
/// FIXME: Rename to `script_context_from_js_context`.
pub fn task_from_context(js_context: *JSContext) -> *mut ScriptTask {
    JS_GetContextPrivate(js_context) as *mut ScriptTask
}

#[unsafe_destructor]
impl Drop for ScriptTask {
    fn finalize(&self) {
        unsafe {
            let _ = local_data::local_data_pop(global_script_context_key);
        }
    }
}

impl ScriptTask {
    /// Creates a new script task.
    pub fn new(compositor: @ScriptListener,
               port: Port<ScriptMsg>,
               chan: ScriptChan,
               engine_chan: EngineChan,
               resource_task: ResourceTask,
               img_cache_task: ImageCacheTask)
               -> @mut ScriptTask {
        let js_runtime = js::rust::rt();
        let js_context = js_runtime.cx();

        js_context.set_default_options_and_version();
        js_context.set_logging_error_reporter();

        let compartment = match js_context.new_compartment(global_class) {
              Ok(c) => c,
              Err(()) => fail!("Failed to create a compartment"),
        };

        let script_task = @mut ScriptTask {
            compositor: compositor,

            layout_map: HashMap::new(),
            image_cache_task: img_cache_task,
            resource_task: resource_task,

            port: port,
            chan: chan,

            engine_chan: engine_chan,

            js_runtime: js_runtime,
            js_context: js_context,
            js_compartment: compartment,

            dom_static: GlobalStaticData(),
            bindings_initialized: false,

            window_size: Size2D(800, 600),
        };
        // Indirection for Rust Issue #6248, dynamic freeze scope artifically extended
        let script_task_ptr = {
            let borrowed_ctx= &mut *script_task;
            borrowed_ctx as *mut ScriptTask
        };
        js_context.set_cx_private(script_task_ptr as *());

        unsafe {
            local_data::local_data_set(global_script_context_key, transmute(script_task))
        }

        script_task
    }

    /// Starts the script task. After calling this method, the script task will loop receiving
    /// messages on its port.
    pub fn start(&mut self) {
        while self.handle_msg() {
            // Go on...
        }
    }

    pub fn create<C: ScriptListener + Owned>(compositor: C,
                                             port: Port<ScriptMsg>,
                                             chan: ScriptChan,
                                             engine_chan: EngineChan,
                                             resource_task: ResourceTask,
                                             image_cache_task: ImageCacheTask) {
        let compositor = Cell(compositor);
        let port = Cell(port);
        // FIXME: rust#6399
        let mut the_task = task();
        the_task.sched_mode(SingleThreaded);
        do spawn {
            let script_task = ScriptTask::new(@compositor.take() as @ScriptListener,
                                              port.take(),
                                              chan.clone(),
                                              engine_chan.clone(),
                                              resource_task.clone(),
                                              image_cache_task.clone());
            script_task.start();
        }
    }

    /// Handles an incoming control message.
    fn handle_msg(&mut self) -> bool {
        match self.port.recv() {
            LoadMsg(id, layout_chan, url) => {
                self.load(id, layout_chan, url);
                true
            }
            ExecuteMsg(url) => {
                self.handle_execute_msg(url);
                true
            }
            SendEventMsg(id, event) => {
                self.handle_event(id, event);
                true
            }
            FireTimerMsg(timer_data) => {
                self.handle_fire_timer_msg(timer_data);
                true
            }
            ReflowCompleteMsg(id) => {
                self.handle_reflow_complete_msg(id);
                true
            }
            ExitMsg => {
                self.handle_exit_msg();
                false
            }
        }
    }

    /// Handles a request to execute a script.
    fn handle_execute_msg(&self, url: Url) {
        debug!("script: Received url `%s` to execute", url::to_str(&url));

        match read_whole_file(&Path(url.path)) {
            Err(msg) => println(fmt!("Error opening %s: %s", url::to_str(&url), msg)),

            Ok(bytes) => {
                self.js_compartment.define_functions(debug_fns);
                let _ = self.js_context.evaluate_script(self.js_compartment.global_obj,
                                                        bytes,
                                                        copy url.path,
                                                        1);
            }
        }
    }

    /// Handles a timer that fired.
    fn handle_fire_timer_msg(&mut self, timer_data: ~TimerData) {
        let this_value = if timer_data.args.len() > 0 {
            RUST_JSVAL_TO_OBJECT(timer_data.args[0])
        } else {
            self.js_compartment.global_obj.ptr
        };

        // TODO: Support extra arguments. This requires passing a `*JSVal` array as `argv`.
        let rval = JSVAL_NULL;
        JS_CallFunctionValue(self.js_context.ptr,
                             this_value,
                             timer_data.funval,
                             0,
                             null(),
                             &rval);

        for self.layout_map.each_value |&layout_info| {
            self.reflow(layout_info, ReflowForScriptQuery)
        }
    }

    /// Handles a notification that reflow completed.
    fn handle_reflow_complete_msg(&mut self, id: uint) {
        self.layout_map.find(&id).get().layout_join_port = None;
        self.compositor.set_ready_state(FinishedLoading);
    }

    /// Handles a request to exit the script task.
    fn handle_exit_msg(&mut self) {
        for self.layout_map.each_value |&layout_info| {
            self.join_layout(layout_info);
            layout_info.root_frame.document.teardown();
        }
    }

    /// The entry point to document loading. Defines bindings, sets up the window and document
    /// objects, parses HTML and CSS, and kicks off initial layout.
    fn load(&mut self, id:uint, layout_chan: LayoutChan, url: Url) {
        // Define the script DOM bindings.
        //
        // FIXME: Can this be done earlier, to save the flag?
        if !self.bindings_initialized {
            define_bindings(self.js_compartment);
            self.bindings_initialized = true
        }

        self.compositor.set_ready_state(Loading);
        // Parse HTML.
        //
        // Note: We can parse the next document in parallel with any previous documents.
        let html_parsing_result = hubbub_html_parser::parse_html(copy url,
                                                                 self.resource_task.clone(),
                                                                 self.image_cache_task.clone());


        let root_node = html_parsing_result.root;

        // Send style sheets over to layout.
        //
        // FIXME: These should be streamed to layout as they're parsed. We don't need to stop here
        // in the script task.
        loop {
              match html_parsing_result.style_port.recv() {
                  Some(sheet) => layout_chan.send(AddStylesheetMsg(sheet)),
                  None => break,
              }
        }

        // Receive the JavaScript scripts.
        let js_scripts = html_parsing_result.js_port.recv();
        debug!("js_scripts: %?", js_scripts);

        // Create the window and document objects.
        let window = Window::new(self.chan.clone(), &mut *self);
        let document = Document(root_node, Some(window));

        // Tie the root into the document.
        do root_node.with_mut_base |base| {
            base.add_to_doc(document)
        }

        // Create the root frame.
        let root_frame = Frame {
            document: document,
            window: window,
            url: url,
        };

        let layout_info = LayoutInfo {
            chan: layout_chan,
            root_frame: root_frame,
            dom_static: GlobalStaticData(),
            layout_join_port: None,
            damage: None,
        };
        self.layout_map.insert(id, layout_info);

        // Perform the initial reflow.
        layout_info.damage = Some(DocumentDamage {
            root: root_node,
            level: MatchSelectorsDocumentDamage,
        });
        self.reflow(layout_info, ReflowForDisplay);

        // Define debug functions.
        self.js_compartment.define_functions(debug_fns);

        // Evaluate every script in the document.
        do vec::consume(js_scripts) |_, bytes| {
            let _ = self.js_context.evaluate_script(self.js_compartment.global_obj,
                                                    bytes,
                                                    ~"???",
                                                    1);
        }
    }

    /// Sends a ping to layout and waits for the response. The response will arrive when the
    /// layout task has finished any pending request messages.
    fn join_layout(&mut self, layout_info: LayoutInfo) {
        if layout_info.layout_join_port.is_some() {
            let join_port = replace(&mut layout_info.layout_join_port, None);
            match join_port {
                Some(ref join_port) => {
                    if !join_port.peek() {
                        info!("script: waiting on layout");
                    }

                    join_port.recv();

                    debug!("script: layout joined")
                }
                None => fail!(~"reader forked but no join port?"),
            }
        }
    }

    /// This method will wait until the layout task has completed its current action, join the
    /// layout task, and then request a new layout run. It won't wait for the new layout
    /// computation to finish.
    ///
    /// This function fails if there is no root frame.
    fn reflow(&mut self, layout_info: LayoutInfo, goal: ReflowGoal) {
        debug!("script: performing reflow");

        // Now, join the layout so that they will see the latest changes we have made.
        self.join_layout(layout_info);

        // Tell the user that we're performing layout.
        self.compositor.set_ready_state(PerformingLayout);

        // Layout will let us know when it's done.
        let (join_port, join_chan) = comm::stream();
        layout_info.layout_join_port = Some(join_port);
        let root_frame = layout_info.root_frame;

        // Send new document and relevant styles to layout.
        let reflow = ~Reflow {
            document_root: root_frame.document.root,
            url: copy root_frame.url,
            goal: goal,
            window_size: self.window_size,
            script_chan: self.chan.clone(),
            script_join_chan: join_chan,
            damage: replace(&mut layout_info.damage, None).unwrap(),
        };

        layout_info.chan.send(ReflowMsg(reflow));

        debug!("script: layout forked")
    }

    /// Reflows the entire document.
    ///
    /// FIXME: This should basically never be used.
    pub fn reflow_all(&mut self, layout_info: LayoutInfo, goal: ReflowGoal) {
        ScriptTask::damage(&mut layout_info.damage,
                           layout_info.root_frame.document.root,
                           MatchSelectorsDocumentDamage);

        self.reflow(layout_info, goal)
    }

    /// Sends the given query to layout.
    pub fn query_layout<T: Owned>(&mut self,
                                  layout_info: LayoutInfo,
                                  query: LayoutQuery,
                                  response_port:
                                  Port<Result<T, ()>>)
                                  -> Result<T,()> {
        self.join_layout(layout_info);
        layout_info.chan.send(QueryMsg(query));
        response_port.recv()
    }

    /// Adds the given damage.
    fn damage(damage: &mut Option<DocumentDamage>,
              root: AbstractNode<ScriptView>,
              level: DocumentDamageLevel) {
        match *damage {
            None => {}
            Some(ref mut damage) => {
                // FIXME(pcwalton): This is wrong. We should trace up to the nearest ancestor.
                damage.root = root;
                damage.level.add(level);
                return
            }
        }

        *damage = Some(DocumentDamage {
            root: root,
            level: level,
        })
    }

    /// This is the main entry point for receiving and dispatching DOM events.
    ///
    /// TODO: Actually perform DOM event dispatch.
    fn handle_event(&mut self, id: uint, event: Event) {
        match event {
            ResizeEvent(new_width, new_height) => {
                debug!("script got resize event: %u, %u", new_width, new_height);

                self.window_size = Size2D(new_width, new_height);

                for self.layout_map.each_value |layout_info| {
                    ScriptTask::damage(&mut layout_info.damage,
                                       layout_info.root_frame.document.root,
                                       ReflowDocumentDamage);

                    self.reflow(*layout_info, ReflowForDisplay)
                }
            }

            // FIXME(pcwalton): This reflows the entire document and is not incremental-y.
            ReflowEvent => {
                debug!("script got reflow event");

                for self.layout_map.each_value |layout_info| {
                    ScriptTask::damage(&mut layout_info.damage,
                                       layout_info.root_frame.document.root,
                                       MatchSelectorsDocumentDamage);

                    self.reflow(*layout_info, ReflowForDisplay)
                }
            }

            ClickEvent(_button, point) => {
                let layout_info = *self.layout_map.find(&id).get();
                debug!("ClickEvent: clicked at %?", point);
                let root = layout_info.root_frame;
                let (port, chan) = comm::stream();
                match self.query_layout(layout_info, HitTestQuery(root.document.root, point, chan), port) {
                    Ok(node) => match node {
                        HitTestResponse(node) => {
                            debug!("clicked on %?", node.debug_str());
                            let mut node = node;
                            // traverse node generations until a node that is an element is found
                            while !node.is_element() {
                                match node.parent_node() {
                                    Some(parent) => {
                                        node = parent;
                                    }
                                    None => break
                                }
                            }
                            if node.is_element() {
                                do node.with_imm_element |element| {
                                    match element.tag_name {
                                        ~"a" => self.load_url_from_element(layout_info, element),
                                        _ => {}
                                    }
                                }
                            }
                        }
                    },
                    Err(()) => {
                        debug!(fmt!("layout query error"));
                    }
                };
            }
            MouseDownEvent(*) => {}
            MouseUpEvent(*) => {}
        }
    }

    priv fn load_url_from_element(&self, layout_info: LayoutInfo, element: &Element) {
        // if the node's element is "a," load url from href attr
        for element.attrs.each |attr| {
            if attr.name == ~"href" {
                debug!("clicked on link to %?", attr.value); 
                let current_url = Some(layout_info.root_frame.url.clone());
                let url = make_url(attr.value.clone(), current_url);
                self.engine_chan.send(LoadUrlMsg(url));
            }
        }
    }
}

