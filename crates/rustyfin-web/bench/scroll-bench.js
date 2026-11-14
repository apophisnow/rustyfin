// Scroll benchmark for poster grids. Paste into the browser console (or run it through a browser
// automation tool) on a page with a grid, then call:
//
//   rfScrollBench.start({ speed: 1500 })   // px per second, from the top to the end of the page
//   rfScrollBench.result()                 // null while running, then a summary object
//
// Every animation frame it scrolls the page by speed/fps and looks at each grid cell that overlaps
// the viewport. A cell counts as "preview" once it paints a BlurHash (`.jf-blurhash`) and as
// "poster" once its <img> has loaded and is not transparent. Cards without a picture are left out.
// The summary has, for the cells that entered the viewport, the time from entering to a preview or
// poster (0 when already shown), the share shown within 100 ms, and the mean share of blank cells
// in the viewport per frame. It also sums the image requests from Resource Timing, with the query
// (which carries api_key) stripped, so no token is ever part of the output.
(() => {
    const state = { running: false, summary: null };

    const percentile = (sorted, p) =>
        sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor((sorted.length - 1) * p))] : null;
    const round = (n) => (n == null || !Number.isFinite(n) ? n : Math.round(n));

    // 'poster', 'preview', 'none' or 'noimage' (a card that shows its type icon, not a picture)
    function cellStatus(cell) {
        if (!cell.classList.contains('jf-card')) return 'none'; // a placeholder awaiting its page
        const img = cell.querySelector('img');
        if (img && img.complete && img.naturalWidth > 0 && getComputedStyle(img).opacity !== '0') return 'poster';
        if (cell.querySelector('.jf-blurhash')) return 'preview';
        if (!img) return 'noimage';
        return 'none';
    }

    function start(opts = {}) {
        const speed = opts.speed || 1500;
        const selector = opts.selector || '#lib-grid > *';
        const settleMs = opts.settleMs ?? 2000;
        const scroller = document.scrollingElement;
        performance.clearResourceTimings();
        performance.setResourceTimingBufferSize(10000);
        scroller.scrollTop = 0;
        state.running = true;
        state.summary = null;

        const cells = new Map(); // grid index -> { enter, preview, poster }
        let blankSum = 0;
        let frames = 0;
        let maxGap = 0;
        let last = null;
        const t0 = performance.now();
        let end = null;

        function frame(now) {
            const t = now - t0;
            if (last != null && end == null) maxGap = Math.max(maxGap, now - last);
            last = now;
            const max = scroller.scrollHeight - innerHeight;
            if (end == null) {
                scroller.scrollTop = Math.min(max, (speed * t) / 1000);
                if (scroller.scrollTop >= max - 1) end = now;
            }
            const grid = document.querySelectorAll(selector);
            let visible = 0;
            let blank = 0;
            for (let i = 0; i < grid.length; i++) {
                const r = grid[i].getBoundingClientRect();
                if (r.bottom <= 0 || r.top >= innerHeight) continue;
                const status = cellStatus(grid[i]);
                if (status === 'noimage') continue;
                visible++;
                let c = cells.get(i);
                if (!c) {
                    c = { enter: t, preview: null, poster: null };
                    cells.set(i, c);
                }
                if (status === 'none') blank++;
                if (status !== 'none' && c.preview == null) c.preview = t;
                if (status === 'poster' && c.poster == null) c.poster = t;
            }
            if (end == null && visible) {
                blankSum += blank / visible;
                frames++;
            }
            if (end != null && now - end >= settleMs) return finish(t);
            requestAnimationFrame(frame);
        }

        function finish(t) {
            const list = [...cells.values()];
            const lat = (key) =>
                list.map((c) => (c[key] == null ? Infinity : c[key] - c.enter)).sort((a, b) => a - b);
            const shown = lat('preview');
            const posters = lat('poster');
            const within = (arr, ms) => (arr.length ? (100 * arr.filter((x) => x <= ms).length) / arr.length : null);
            const images = performance
                .getEntriesByType('resource')
                .filter((e) => /\/Items\/[^/]+\/Images\//.test(new URL(e.name).pathname));
            // Through a service worker, transferSize is 0 whether its cache or the network answered
            const viaWorker = images.filter((e) => e.workerStart > 0);
            const cached = images.filter((e) => !e.workerStart && e.transferSize === 0 && e.decodedBodySize > 0);
            const ttfb = images
                .filter((e) => !cached.includes(e) && e.responseStart > 0)
                .map((e) => e.responseStart - e.requestStart)
                .sort((a, b) => a - b);
            state.summary = {
                speedPxPerS: speed,
                viewport: `${innerWidth}x${innerHeight}@${devicePixelRatio}`,
                scrollMs: round(t - settleMs),
                frames,
                maxFrameGapMs: round(maxGap),
                cellsEntered: list.length,
                shownWithin100msPct: round(within(shown, 100)),
                posterWithin100msPct: round(within(posters, 100)),
                shownP50Ms: round(percentile(shown, 0.5)),
                shownP95Ms: round(percentile(shown, 0.95)),
                posterP50Ms: round(percentile(posters, 0.5)),
                posterP95Ms: round(percentile(posters, 0.95)),
                neverPoster: posters.filter((x) => x === Infinity).length,
                blankInViewportPct: frames ? round((100 * blankSum) / frames) : null,
                imageRequests: images.length,
                imageRequestsFromCache: cached.length,
                imageRequestsViaServiceWorker: viaWorker.length,
                imageKB: round(images.reduce((s, e) => s + (e.transferSize || 0), 0) / 1024),
                imageTtfbP50Ms: round(percentile(ttfb, 0.5)),
                imageTtfbP95Ms: round(percentile(ttfb, 0.95)),
            };
            state.running = false;
        }

        requestAnimationFrame(frame);
        return 'started';
    }

    window.rfScrollBench = { start, result: () => (state.running ? null : state.summary) };
})();
