/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use dom::bindings::codegen::HTMLCollectionBinding;
use dom::bindings::utils::{CacheableWrapper, BindingObject, WrapperCache};
use dom::htmlcollection::HTMLCollection;
use dom::window::Window;
use script_task::{global_script_context, LayoutInfo};

use js::jsapi::{JSObject, JSContext};

pub impl HTMLCollection {
    fn init_wrapper(@mut self, owner: &Window) {
        let script_context = global_script_context();
        let cx = script_context.js_compartment.cx.ptr;
        let cache = owner.get_wrappercache();
        let scope = cache.get_wrapper();
        self.wrap_object_shared(cx, scope);
    }
}

impl BindingObject for HTMLCollection {
    fn GetParentObject(&self, layout_info: *LayoutInfo) -> @mut CacheableWrapper {
        unsafe {
            (*layout_info).root_frame.window as @mut CacheableWrapper
        }
    }
}

impl CacheableWrapper for HTMLCollection {
    fn get_wrappercache(&mut self) -> &mut WrapperCache {
        unsafe {
            cast::transmute(&self.wrapper)
        }
    }

    fn wrap_object_shared(@mut self, cx: *JSContext, scope: *JSObject) -> *JSObject {
        let mut unused = false;
        HTMLCollectionBinding::Wrap(cx, scope, self, &mut unused)
    }
}
