/**
 * Hot App - Auto-Refresh Foundation
 *
 * Shared infrastructure for consistent live data refresh across the app.
 * Exposed as `window.HotRefresh`.
 *
 * Responsibilities:
 * - Page refresh-region registry: pages register named regions (desktop table,
 *   mobile card, modal/inspector, graph) each with their own refresh function.
 * - Scheduler: coalesces bursts of triggers (SSE events + polling) using a
 *   debounce, and protects expensive pages with a minimum interval (throttle).
 * - Page Visibility: pauses refreshes while the tab is hidden and performs a
 *   single catch-up refresh on refocus.
 * - Request lifecycle helpers: `refreshHtmlTarget` and `fetchJson` guard against
 *   stale/out-of-order responses, disable caching, and handle auth redirects.
 *
 * The SSE indicator (components/sse_status_indicator.html) calls
 * `HotRefresh.trigger()` instead of invoking page callbacks directly. Pages that
 * have not yet been migrated keep working via the legacy `window.sseRefreshCallback`.
 */

(function () {
    'use strict';

    // Debounce window: collapse short bursts of triggers into one refresh.
    var DEBOUNCE_MS = 400;
    // Throttle floor: never run full refresh cycles closer together than this.
    var THROTTLE_MS = 1500;

    // Registered refresh regions, keyed by name.
    var regions = new Map();

    // Per-element/key request generation counters, so a slow older response
    // cannot overwrite a newer one.
    var generations = new WeakMap();
    var keyedGenerations = new Map();

    var debounceTimer = null;
    var lastRunAt = 0;
    var pendingWhileHidden = false;

    // ============================================
    // Region registry
    // ============================================

    /**
     * Register a refresh region.
     * @param {string} name unique region id (e.g. "run-header", "stream-graph").
     * @param {Function} fn refresh function; may return a Promise.
     * @param {Object} [options] reserved for future per-region tuning.
     */
    function registerRegion(name, fn, options) {
        if (!name || typeof fn !== 'function') return;
        regions.set(name, { name: name, fn: fn, options: options || {} });
    }

    function unregisterRegion(name) {
        regions.delete(name);
    }

    function clearRegions() {
        regions.clear();
    }

    function runRegions(reason) {
        regions.forEach(function (region) {
            try {
                region.fn(reason);
            } catch (e) {
                if (window.console && console.warn) {
                    console.warn('HotRefresh region "' + region.name + '" failed', e);
                }
            }
        });

        // Back-compat: pages not yet migrated to the registry still expose a
        // single global callback.
        if (typeof window.sseRefreshCallback === 'function') {
            try {
                window.sseRefreshCallback(reason);
            } catch (e) {
                if (window.console && console.warn) {
                    console.warn('HotRefresh sseRefreshCallback failed', e);
                }
            }
        }
    }

    // ============================================
    // Scheduler (debounce + throttle + visibility)
    // ============================================

    function trigger(reason) {
        // Pause while hidden; remember that work is pending for refocus.
        if (document.hidden) {
            pendingWhileHidden = true;
            return;
        }
        if (debounceTimer) clearTimeout(debounceTimer);
        debounceTimer = setTimeout(function () {
            fire(reason);
        }, DEBOUNCE_MS);
    }

    function fire(reason) {
        debounceTimer = null;

        if (document.hidden) {
            pendingWhileHidden = true;
            return;
        }

        var now = Date.now();
        var sinceLast = now - lastRunAt;
        if (sinceLast < THROTTLE_MS) {
            // Too soon since the last refresh; wait out the remaining interval.
            debounceTimer = setTimeout(function () {
                fire(reason);
            }, THROTTLE_MS - sinceLast);
            return;
        }

        lastRunAt = now;
        runRegions(reason);
    }

    /** Force an immediate refresh, bypassing debounce (still respects hidden). */
    function refreshNow(reason) {
        if (debounceTimer) {
            clearTimeout(debounceTimer);
            debounceTimer = null;
        }
        if (document.hidden) {
            pendingWhileHidden = true;
            return;
        }
        lastRunAt = Date.now();
        runRegions(reason);
    }

    document.addEventListener('visibilitychange', function () {
        if (!document.hidden && pendingWhileHidden) {
            pendingWhileHidden = false;
            // One catch-up refresh after returning to the tab.
            trigger('visible');
        }
    });

    // ============================================
    // Request lifecycle helpers
    // ============================================

    function nextGen(token) {
        if (typeof token === 'string') {
            var g = (keyedGenerations.get(token) || 0) + 1;
            keyedGenerations.set(token, g);
            return g;
        }
        var current = (generations.get(token) || 0) + 1;
        generations.set(token, current);
        return current;
    }

    function isCurrentGen(token, gen) {
        if (typeof token === 'string') {
            return keyedGenerations.get(token) === gen;
        }
        return generations.get(token) === gen;
    }

    /**
     * Detect and follow auth/session redirects so a refresh never silently
     * swaps login HTML into a data region.
     * @returns {boolean} true if a redirect was handled (caller should stop).
     */
    function handleAuthRedirect(resp) {
        var hxRedirect = resp.headers.get('HX-Redirect');
        if (hxRedirect) {
            window.location.href = hxRedirect;
            return true;
        }
        if (resp.redirected && /\/sign(in|up)\b/.test(resp.url)) {
            window.location.href = resp.url;
            return true;
        }
        if (resp.status === 401) {
            window.location.href = '/signin';
            return true;
        }
        return false;
    }

    function applyFormattingSafe(el) {
        if (typeof window.applyFormatting === 'function') {
            try {
                window.applyFormatting(el);
            } catch (e) {
                /* non-fatal */
            }
        }
    }

    /**
     * Refresh a server-rendered HTML region. Stale responses are ignored, the
     * last known-good DOM is preserved on error, and auth redirects are handled.
     *
     * @param {string} url fragment URL (use /partials/... routes).
     * @param {string|Element} target selector or element.
     * @param {Object} [options] { swap: 'innerHTML' | 'outerHTML' }
     */
    function refreshHtmlTarget(url, target, options) {
        options = options || {};
        var el = typeof target === 'string' ? document.querySelector(target) : target;
        if (!el) return Promise.resolve(false);

        var gen = nextGen(el);
        return fetch(url, {
            credentials: 'same-origin',
            cache: 'no-store',
            headers: {
                'HX-Request': 'true',
                'X-Requested-With': 'fetch',
            },
        })
            .then(function (resp) {
                if (!isCurrentGen(el, gen)) return false;
                if (handleAuthRedirect(resp)) return false;
                if (!resp.ok) return false; // keep last known-good
                return resp.text().then(function (html) {
                    if (!isCurrentGen(el, gen)) return false;
                    if (options.swap === 'outerHTML') {
                        var parent = el.parentElement;
                        el.outerHTML = html;
                        applyFormattingSafe(parent || document.body);
                    } else {
                        el.innerHTML = html;
                        applyFormattingSafe(el);
                    }
                    return true;
                });
            })
            .catch(function () {
                // Network error: preserve current UI, let the next trigger retry.
                return false;
            });
    }

    /**
     * Fetch JSON for a refresh, with stale-response protection and auth handling.
     * Returns the parsed data, or null when the response is stale/failed/redirected.
     *
     * @param {string} url JSON URL (use /data/... routes).
     * @param {Object} [options] { key: string } to dedupe by logical region.
     */
    function fetchJson(url, options) {
        options = options || {};
        var token = options.key || url;
        var gen = nextGen(token);
        return fetch(url, {
            credentials: 'same-origin',
            cache: 'no-store',
            headers: { 'X-Requested-With': 'fetch' },
        })
            .then(function (resp) {
                if (!isCurrentGen(token, gen)) return null;
                if (handleAuthRedirect(resp)) return null;
                if (!resp.ok) return null;
                return resp.json().then(function (data) {
                    if (!isCurrentGen(token, gen)) return null;
                    return data;
                });
            })
            .catch(function () {
                return null;
            });
    }

    // ============================================
    // Export
    // ============================================

    window.HotRefresh = {
        registerRegion: registerRegion,
        unregisterRegion: unregisterRegion,
        clearRegions: clearRegions,
        trigger: trigger,
        refreshNow: refreshNow,
        refreshHtmlTarget: refreshHtmlTarget,
        fetchJson: fetchJson,
        // Tuning knobs exposed for tests/diagnostics.
        _config: { debounceMs: DEBOUNCE_MS, throttleMs: THROTTLE_MS },
    };
})();
