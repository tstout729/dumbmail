import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { Maximize, Minus, Plus } from "lucide-react";
import { cx } from "../ui";

// x is the content's horizontal center relative to the viewport's center, y is its top edge.
type View = { x: number; y: number; scale: number };
type Point = { x: number; y: number };
type Gesture =
  | { kind: "pan"; start: Point; origin: View; moved: boolean }
  | { kind: "pinch"; dist: number; mid: Point; origin: View };
// Safari's trackpad pinch (non-standard GestureEvent).
type SafariGesture = Event & { scale: number; clientX: number; clientY: number };

const MIN_SCALE = 0.25;
const MAX_SCALE = 1.5;
const DRAG_THRESHOLD = 4;
const HOME: View = { x: 0, y: 0, scale: 1 };

const clampScale = (s: number) => Math.min(MAX_SCALE, Math.max(MIN_SCALE, s));
const distance = (a: Point, b: Point) => Math.hypot(a.x - b.x, a.y - b.y);
const midpoint = (a: Point, b: Point): Point => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });

/** Rescales `from` to `scale`, keeping the content point under `focus` (viewport coordinates) in place. */
function zoomAround(from: View, scale: number, focus: Point, viewportWidth: number): View {
  const next = clampScale(scale);
  const ratio = next / from.scale;
  const cx = viewportWidth / 2;
  return { scale: next, x: focus.x - cx - (focus.x - cx - from.x) * ratio, y: focus.y - (focus.y - from.y) * ratio };
}

/**
 * A canvas you can drag around and zoom: drag anywhere (a drag never counts as a click on a step),
 * scroll or two-finger swipe to pan, pinch or Ctrl+scroll to zoom.
 */
export function PanZoom({ children }: { children: ReactNode }) {
  const viewportRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const [view, setView] = useState<View>(HOME);
  const [dragging, setDragging] = useState(false);
  const pointers = useRef(new Map<number, Point>());
  const gesture = useRef<Gesture | null>(null);
  const swallowClick = useRef(false);

  const local = (clientX: number, clientY: number): Point => {
    const r = viewportRef.current?.getBoundingClientRect();
    return { x: clientX - (r?.left ?? 0), y: clientY - (r?.top ?? 0) };
  };
  const width = () => viewportRef.current?.clientWidth ?? 0;

  // External system: wheel and Safari gesture events must be non-passive to stop the page scrolling or zooming.
  useEffect(() => {
    const el = viewportRef.current;
    if (!el) return;
    const toLocal = (clientX: number, clientY: number): Point => {
      const r = el.getBoundingClientRect();
      return { x: clientX - r.left, y: clientY - r.top };
    };
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? el.clientHeight : 1;
      if (e.ctrlKey || e.metaKey) {
        const focus = toLocal(e.clientX, e.clientY);
        // Trackpad pinches send small deltas; cap mouse-wheel notches so one notch is a gentle step.
        const delta = Math.max(-25, Math.min(25, e.deltaY * unit));
        setView((v) => zoomAround(v, v.scale * Math.exp(-delta * 0.01), focus, el.clientWidth));
        return;
      }
      const horizontal = e.shiftKey && e.deltaX === 0;
      const dx = (horizontal ? e.deltaY : e.deltaX) * unit;
      const dy = horizontal ? 0 : e.deltaY * unit;
      setView((v) => ({ ...v, x: v.x - dx, y: v.y - dy }));
    };
    // Safari reports pinch scale relative to the gesture start; apply it as a step since the last event.
    let lastScale = 1;
    const onGestureStart = (e: Event) => {
      e.preventDefault();
      lastScale = 1;
    };
    const onGestureChange = (e: Event) => {
      e.preventDefault();
      const g = e as SafariGesture;
      const step = g.scale / lastScale;
      lastScale = g.scale;
      const focus = toLocal(g.clientX, g.clientY);
      setView((v) => zoomAround(v, v.scale * step, focus, el.clientWidth));
    };
    const onGestureEnd = (e: Event) => e.preventDefault();
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("gesturestart", onGestureStart);
    el.addEventListener("gesturechange", onGestureChange);
    el.addEventListener("gestureend", onGestureEnd);
    return () => {
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("gesturestart", onGestureStart);
      el.removeEventListener("gesturechange", onGestureChange);
      el.removeEventListener("gestureend", onGestureEnd);
    };
  }, []);

  const startPinch = () => {
    const [a, b] = [...pointers.current.values()];
    if (!a || !b) return;
    gesture.current = { kind: "pinch", dist: distance(a, b), mid: midpoint(a, b), origin: view };
    swallowClick.current = true;
  };

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.pointerType === "mouse" && e.button !== 0) return;
    if (pointers.current.size === 0) swallowClick.current = false;
    const p = local(e.clientX, e.clientY);
    pointers.current.set(e.pointerId, p);
    if (pointers.current.size === 2) startPinch();
    else if (pointers.current.size === 1) gesture.current = { kind: "pan", start: p, origin: view, moved: false };
  };

  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!pointers.current.has(e.pointerId)) return;
    const p = local(e.clientX, e.clientY);
    pointers.current.set(e.pointerId, p);
    const g = gesture.current;
    if (!g) return;
    if (g.kind === "pan") {
      const dx = p.x - g.start.x;
      const dy = p.y - g.start.y;
      if (!g.moved) {
        if (Math.hypot(dx, dy) < DRAG_THRESHOLD) return;
        g.moved = true;
        swallowClick.current = true;
        e.currentTarget.setPointerCapture(e.pointerId);
        setDragging(true);
      }
      setView({ ...g.origin, x: g.origin.x + dx, y: g.origin.y + dy });
      return;
    }
    const [a, b] = [...pointers.current.values()];
    if (!a || !b) return;
    const mid = midpoint(a, b);
    const zoomed = zoomAround(g.origin, (g.origin.scale * distance(a, b)) / g.dist, g.mid, width());
    setView({ ...zoomed, x: zoomed.x + mid.x - g.mid.x, y: zoomed.y + mid.y - g.mid.y });
  };

  const onPointerEnd = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!pointers.current.delete(e.pointerId)) return;
    if (pointers.current.size === 1) {
      // Lifting one finger of a pinch carries on as a pan with the other.
      const [p] = [...pointers.current.values()];
      if (p) gesture.current = { kind: "pan", start: p, origin: view, moved: true };
      return;
    }
    if (pointers.current.size === 0) {
      gesture.current = null;
      setDragging(false);
    }
  };

  const zoomBy = (factor: number) => {
    const el = viewportRef.current;
    if (!el) return;
    setView((v) => zoomAround(v, v.scale * factor, { x: el.clientWidth / 2, y: el.clientHeight / 2 }, el.clientWidth));
  };

  const fit = () => {
    const el = viewportRef.current;
    const content = contentRef.current;
    if (!el || !content) return;
    const w = content.offsetWidth;
    const h = content.offsetHeight;
    const scale = clampScale(Math.min(1, el.clientWidth / w, el.clientHeight / h));
    setView({ x: 0, y: Math.max(0, (el.clientHeight - h * scale) / 2), scale });
  };

  const dot = 18 * view.scale;
  return (
    <div
      ref={viewportRef}
      className={cx("relative h-full touch-none select-none overflow-hidden", dragging ? "cursor-grabbing" : "cursor-grab")}
      style={{
        backgroundImage: "radial-gradient(circle, #e4e4e7 1px, transparent 1px)",
        backgroundSize: `${dot}px ${dot}px`,
        backgroundPosition: `calc(50% + ${view.x}px) ${view.y}px`,
      }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerEnd}
      onPointerCancel={onPointerEnd}
      onClickCapture={(e) => {
        if (!swallowClick.current) return;
        swallowClick.current = false;
        e.preventDefault();
        e.stopPropagation();
      }}
    >
      <div
        className="absolute left-1/2 top-0 w-0 origin-top-left"
        style={{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.scale})` }}
      >
        <div ref={contentRef} className="absolute left-0 top-0 w-max -translate-x-1/2 [&_button]:cursor-pointer">
          {children}
        </div>
      </div>
      <div
        className="absolute bottom-4 right-4 z-10 flex items-center rounded-lg border border-zinc-200 bg-white p-0.5 shadow-sm"
        onPointerDown={(e) => e.stopPropagation()}
      >
        <ZoomButton label="Zoom out" onClick={() => zoomBy(1 / 1.25)} disabled={view.scale <= MIN_SCALE}>
          <Minus className="size-3.5" />
        </ZoomButton>
        <button
          type="button"
          onClick={() => setView(HOME)}
          title="Reset to 100%"
          className="min-w-12 cursor-pointer rounded-md px-1.5 py-1 text-xs tabular-nums text-zinc-600 hover:bg-zinc-100"
        >
          {Math.round(view.scale * 100)}%
        </button>
        <ZoomButton label="Zoom in" onClick={() => zoomBy(1.25)} disabled={view.scale >= MAX_SCALE}>
          <Plus className="size-3.5" />
        </ZoomButton>
        <span className="mx-0.5 h-4 w-px bg-zinc-200" />
        <ZoomButton label="Fit to screen" onClick={fit}>
          <Maximize className="size-3.5" />
        </ZoomButton>
      </div>
    </div>
  );
}

function ZoomButton({ label, onClick, disabled, children }: { label: string; onClick: () => void; disabled?: boolean; children: ReactNode }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      disabled={disabled}
      className="flex size-7 cursor-pointer items-center justify-center rounded-md text-zinc-600 hover:bg-zinc-100 disabled:cursor-default disabled:opacity-40"
    >
      {children}
    </button>
  );
}
