        // ─── Info-panel dragging ────────────────────────────

        // Info panel is docked to the right side — drag is no longer applicable.

        // Escapes as *text*, not as an attribute value. Round-tripping
        // through a text node replaces `&`, `<` and `>` and nothing else, so
        // a quote comes back untouched — and most callers here drop the
        // result inside a double-quoted attribute (`title="…"`,
        // `data-id="…"`). No indexed name has carried a quote yet, which is
        // the only reason that holds. If you ever escape something a user
        // types rather than something the indexer extracted, this is not the
        // function for it.
        //
        // Pinned by `tests/js/interaction.mjs`, which asserts the quote
        // survives so the limit is visible rather than assumed.
        function escapeHtml(text) {
            if (text == null) return '';
            const div = document.createElement('div');
            div.textContent = text;
            return div.innerHTML;
        }

        window.addEventListener('resize', () => {
            width = document.getElementById('container').clientWidth;
            height = document.getElementById('container').clientHeight;
            resizeRenderer(width, height);
            // The FX overlay sizes its canvas inside its own draw, and that
            // draw is now allowed to idle — so a resized window would keep the
            // old, stretched canvas until something else moved.
            overlayInvalidate();
        });

        document.addEventListener('keydown', e => {
            // Don't hijack keys while the user is typing in a field.
            const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(e.target.tagName) || e.target.isContentEditable;

            if (e.key === 'Escape') {
                if (state.pathMode) { exitPathMode(); return; }
                if (state.focusNode || state.selectedNode) { clearSelection(); frameGraph(700); return; }
            }
            if (typing) return;

            // Back / forward through visited nodes.
            if (e.key === 'Backspace') {
                e.preventDefault();
                navHistory(e.shiftKey ? 1 : -1);
                return;
            }
            // Step the selection through the focused node's neighbours.
            if (e.key === 'Tab' && (state.focusNode || state.selectedNode)) {
                e.preventDefault();
                cycleNeighbor(e.shiftKey ? -1 : 1);
                return;
            }

            // Number keys 1–6 snap to the face projections; 0 returns to 3D.
            const viewId = (e.key === '0') ? '3d' : (VIEWS[e.key] ? e.key : null);
            if (viewId) {
                setActiveViewBtn(viewId);
                setView(viewId);
            }
        });

        bootstrap();
