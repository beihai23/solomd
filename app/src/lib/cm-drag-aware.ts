/**
 * Pointer-drag awareness for CodeMirror 6 view plugins.
 *
 * Several of our plugins (`cm-live-preview`, `cm-live-blocks`,
 * `cm-live-render`) rebuild their decoration set on every `selectionSet`
 * update so markdown markers or widgets toggle as the cursor crosses lines.
 * That works fine for keyboard movement and single clicks, but during a
 * mouse drag-selection it tears down and re-mounts DOM nodes (especially
 * `Decoration.replace` widgets with `contenteditable="false"`) under the
 * pointer. On Windows WebView2 that mid-drag mutation breaks pointer
 * capture — the OS focus leaves the editor and the drag aborts.
 *
 * This module owns one editor-wide "is the user currently drag-selecting"
 * flag, exposed via `isDragging(state)`. Plugins call it in their
 * `update()` and skip selection-only rebuilds while it's true. When the
 * pointer is released we dispatch `dragEndEffect`; plugins should treat
 * that as a forced rebuild so widgets reflect the final selection.
 *
 * Wire `dragAwareExtension()` into the editor once (Editor.vue) and any
 * number of plugins can read it.
 */

import { StateEffect, StateField, Transaction } from '@codemirror/state';
import type { EditorState } from '@codemirror/state';
import { EditorView, ViewPlugin } from '@codemirror/view';

// `dragStartEffect` is exported only so dev / self-test code can verify the
// field is wired; production code never imports it directly.
export const dragStartEffect = StateEffect.define<null>();
export const dragEndEffect = StateEffect.define<null>();

const dragField = StateField.define<boolean>({
  create: () => false,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(dragStartEffect)) return true;
      if (e.is(dragEndEffect)) return false;
    }
    return value;
  },
});

const dragTracker = ViewPlugin.fromClass(
  class {
    cleanup: (() => void) | null = null;

    constructor(view: EditorView) {
      const onDown = (e: PointerEvent) => {
        // Touch drags don't reproduce the WebView2 capture loss and CM has
        // its own touch handling; only guard mouse / pen.
        if (e.pointerType === 'touch') return;
        if (!view.state.field(dragField, false)) {
          view.dispatch({ effects: dragStartEffect.of(null) });
        }
      };
      const onUp = () => {
        if (view.state.field(dragField, false)) {
          view.dispatch({ effects: dragEndEffect.of(null) });
        }
      };
      view.scrollDOM.addEventListener('pointerdown', onDown);
      // `window` (not `document`) so we still see the release if the user
      // dragged out of the editor onto the scrollbar / titlebar / menubar.
      window.addEventListener('pointerup', onUp);
      window.addEventListener('pointercancel', onUp);
      this.cleanup = () => {
        view.scrollDOM.removeEventListener('pointerdown', onDown);
        window.removeEventListener('pointerup', onUp);
        window.removeEventListener('pointercancel', onUp);
      };
    }

    destroy() {
      this.cleanup?.();
      this.cleanup = null;
    }
  },
);

export function dragAwareExtension() {
  return [dragField, dragTracker];
}

export function isDragging(state: EditorState): boolean {
  return state.field(dragField, false) ?? false;
}

export function isDragEndTransaction(tr: Transaction): boolean {
  return tr.effects.some((e) => e.is(dragEndEffect));
}
