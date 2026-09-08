// Justified row layout (design-refresh-plan §3.3): equal-height, variable-width
// rows that preserve each item's original aspect ratio.
//
// Pure math only — no DOM access — so node --test can drive it directly.
// The caller supplies item aspect ratios; this module never measures images.

export const JUSTIFIED_GAP_PX = 4;
export const JUSTIFIED_FALLBACK_ASPECT = 1.33;
// File-type cards without intrinsic dimensions (ZIP/TXT/source) fall back to a
// squarer cell: a 4:3 placeholder read as the row's visual center next to
// portrait artwork, doubling its width for one small icon (fix plan I).
export const JUSTIFIED_FILE_FALLBACK_ASPECT = 1;

export function justifiedAspect(item) {
  const width = Number(item && item.width);
  const height = Number(item && item.height);
  if (Number.isFinite(width) && Number.isFinite(height) && width > 0 && height > 0) {
    return width / height;
  }
  const mediaType = item && (item.media_type || (item.is_archive ? 'archive' : 'image'));
  if (mediaType === 'archive' || mediaType === 'text' || mediaType === 'source') {
    return JUSTIFIED_FILE_FALLBACK_ASPECT;
  }
  return JUSTIFIED_FALLBACK_ASPECT;
}

// Row height tiers (§3.3): gallery 200px / compact 140px on desktop, mobile 120px.
// The mobile column-count toggle is NOT a row-height contract: on mobile only
// 1 column uses this justified engine (full-width natural-aspect rows); 2/3
// columns render the fixed CSS grid in views.css, which repeats
// var(--mobile-grid-columns) so the chosen count is the real slot count.
export const JUSTIFIED_ROW_HEIGHTS = {
  desktop: {grid: 200, compact: 140},
  mobile: {grid: 120, compact: 84},
};

export function justifiedRowHeight(view, options = {}) {
  const breakpoint = options.mobile ? 'mobile' : 'desktop';
  const tiers = JUSTIFIED_ROW_HEIGHTS[breakpoint] || JUSTIFIED_ROW_HEIGHTS.desktop;
  return tiers[view] || tiers.grid;
}

// Build equal-height rows from items. A row closes once the accumulated aspect
// sum reaches containerWidth/targetRowHeight (§3.3 step 2); each member then
// renders at width = usable × aspect_i / Σaspect (usable = container width minus
// the gaps taken out first, step 3). The trailing row keeps the target height and
// natural widths — left-aligned, never stretched (step 4). mobileColumns=1 lays
// every item out as its own full-width row at natural aspect; that branch must
// run before the targetRowHeight guard so valid content can never be dropped
// just because the caller's row-height tier is unused (or 0) in single-column
// mode.
export function computeJustifiedRows(items, containerWidth, targetRowHeight, options = {}) {
  const list = (items || []).filter(Boolean);
  if (options.mobileColumns === 1) {
    if (containerWidth <= 0) return [];
    return list.map(item => {
      const aspect = justifiedAspect(item);
      return {
        items: [{item, width: containerWidth, height: containerWidth / aspect}],
        height: containerWidth / aspect,
        single: true,
      };
    });
  }
  if (containerWidth <= 0 || targetRowHeight <= 0) return [];
  const gap = options.gap === undefined ? JUSTIFIED_GAP_PX : options.gap;
  const rows = [];
  let current = [];
  let aspectSum = 0;
  const threshold = containerWidth / targetRowHeight;
  for (const item of list) {
    const aspect = justifiedAspect(item);
    current.push({item, aspect});
    aspectSum += aspect;
    if (aspectSum >= threshold) {
      const usable = containerWidth - gap * (current.length - 1);
      const height = usable / aspectSum;
      rows.push({
        items: current.map(entry => ({
          item: entry.item,
          width: entry.aspect * height,
          height,
        })),
        height,
      });
      current = [];
      aspectSum = 0;
    }
  }
  if (current.length) {
    rows.push({
      items: current.map(entry => ({
        item: entry.item,
        width: entry.aspect * targetRowHeight,
        height: targetRowHeight,
      })),
      height: targetRowHeight,
      trailing: true,
    });
  }
  return rows;
}
