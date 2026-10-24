import { describe, expect, it } from "vitest";
import {
  basename,
  describeCombine,
  describeDelete,
  describeExport,
  describePrune,
  describeTrim,
  filterClips,
  gameOptions,
  parseTagInput,
  pruneThumbnails,
  shouldStartDrag,
  tagOptions,
  resolveSelection,
  thumbnailAtMs,
  toClipView,
  toClipViews,
} from "./clips";
import type { ClipView } from "./clips";
import type { ClipDto, DeleteOutcome, PruneOutcome } from "./types";

const assetUrl = (path: string) =>
  `asset://localhost/${encodeURIComponent(path)}`;

function clip(overrides: Partial<ClipDto> = {}): ClipDto {
  return {
    id: 1,
    kind: 'clip',
    path: "/home/player/.local/share/replayed/clips/clip-1.mp4",
    started_at_ms: 0,
    duration_ms: 12_345,
    size_bytes: 812 * 1024,
    codec: "h264_nvenc",
    favourite: false,
    created_at_ms: 1_756_000_000_000,
    title: null,
    game: null,
    tags: [],
    ...overrides,
  };
}

describe("basename", () => {
  // The application runs on Windows and indexes the paths it wrote there, while the
  // development host is macOS: a split that only knew about `/` would render every clip's
  // name as its whole path.
  it.each([
    ["/home/player/clips/clip-1.mp4", "clip-1.mp4"],
    ["C:\\Users\\player\\Videos\\replayed\\clip.mp4", "clip.mp4"],
    ["C:\\mixed\\separators/clip.mp4", "clip.mp4"],
    ["clip.mp4", "clip.mp4"],
    ["", ""],
    ["/home/player/clips/", "/home/player/clips/"], // a trailing separator is not a name
  ])("basename(%j) is %j", (path, name) => {
    expect(basename(path)).toBe(name);
  });
});

describe("thumbnailAtMs", () => {
  it.each([
    [60_000, 1_000], // a frame one second in
    [2_000, 1_000],
    [1_000, 500], // the middle of anything one second or shorter
    [800, 400],
    [1, 0],
    [0, 0],
    [-1_000, 0],
    [Number.NaN, 0],
  ])("thumbnailAtMs(%d) is %d", (duration, at) => {
    expect(thumbnailAtMs(duration)).toBe(at);
  });

  it("never asks for a frame that is not there", () => {
    for (const duration of [1, 250, 1_000, 5_000, 1_800_000]) {
      expect(thumbnailAtMs(duration)).toBeLessThan(duration);
    }
  });
});

describe("toClipView", () => {
  it("maps the row the store holds into what the list shows", () => {
    const view = toClipView(clip(), assetUrl);

    expect(view.id).toBe(1);
    expect(view.name).toBe("clip-1.mp4");
    expect(view.path).toBe(
      "/home/player/.local/share/replayed/clips/clip-1.mp4",
    );
    expect(view.assetUrl).toBe(
      assetUrl("/home/player/.local/share/replayed/clips/clip-1.mp4"),
    );
    expect(view.durationMs).toBe(12_345);
    expect(view.durationLabel).toBe("0:12.3");
    expect(view.sizeBytes).toBe(812 * 1024);
    expect(view.sizeLabel).toBe("812 KiB");
    expect(view.codec).toBe("h264_nvenc");
    expect(view.favourite).toBe(false);
    expect(toClipView(clip({ favourite: true }), assetUrl).favourite).toBe(
      true,
    );
    expect(view.createdAtMs).toBe(1_756_000_000_000);
    // Local time, so only the shape is pinned.
    expect(view.createdAtLabel).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  });

  it("builds the playback URL through the injected asset-protocol mapping", () => {
    // Playback is the asset protocol and nothing else: the view never hands the raw path to
    // a <video> element, because a bare filesystem path is not something a webview can load.
    const view = toClipView(clip({ path: "C:\\clips\\a b.mp4" }), assetUrl);
    expect(view.assetUrl).toContain(assetUrl("C:\\clips\\a b.mp4"));
    expect(view.assetUrl).not.toBe(view.path);
  });
});

describe("toClipViews", () => {
  it("preserves the order the store returned", () => {
    // Newest first is decided in Rust, on the wall-clock column, and is not second-guessed
    // here: re-sorting by `started_at_ms` in the UI would silently disagree with it.
    const rows = [
      clip({ id: 3, started_at_ms: 10 }),
      clip({ id: 2, started_at_ms: 99_999 }),
      clip({ id: 1, started_at_ms: 0 }),
    ];
    expect(toClipViews(rows, assetUrl).map((view) => view.id)).toEqual([
      3, 2, 1,
    ]);
  });

  it("maps an empty list to an empty list", () => {
    expect(toClipViews([], assetUrl)).toEqual([]);
  });
});

describe("resolveSelection", () => {
  const views: ClipView[] = toClipViews(
    [clip({ id: 5 }), clip({ id: 4 })],
    assetUrl,
  );

  it("keeps a selection that is still in the list", () => {
    expect(resolveSelection(views, 4)).toBe(4);
  });

  it("falls back to the newest clip when the selected one is gone", () => {
    // Deleting the clip that was open, or a cleanup pass that removed it, must not leave
    // the detail pane pointing at a row that no longer exists.
    expect(resolveSelection(views, 99)).toBe(5);
  });

  it("selects nothing when there is nothing to select", () => {
    expect(resolveSelection([], null)).toBeNull();
    expect(resolveSelection([], 5)).toBeNull();
  });

  it("picks the newest clip on a first open", () => {
    expect(resolveSelection(views, null)).toBe(5);
  });
});

describe("describeTrim", () => {
  it("reports a trim whose length matches what was asked for", () => {
    const written = clip({
      id: 7,
      path: "/clips/game.trim-1500-3000.mp4",
      duration_ms: 1_500,
    });
    const message = describeTrim({ startMs: 1_500, endMs: 3_000 }, written);

    expect(message).toContain("game.trim-1500-3000.mp4");
    expect(message).toContain("0:01.5");
    expect(message).toContain("clip #7");
    expect(message).toContain("losslessly remuxed");
  });

  it("shows both numbers when a stream copy cut somewhere else", () => {
    // The point of the whole report: a stream copy cannot always cut where it was asked to,
    // and rounding the two numbers into agreement would hide that.
    const written = clip({ id: 8, path: "/clips/cut.mp4", duration_ms: 2_450 });
    const message = describeTrim({ startMs: 0, endMs: 3_000 }, written);

    expect(message).toContain("asked for 0:03.0");
    expect(message).toContain("0:02.5");
    expect(message).toContain("stream copy cuts where the stream allows");
  });

  it("treats a difference within a couple of frames as equal", () => {
    const written = clip({
      id: 9,
      path: "/clips/near.mp4",
      duration_ms: 3_010,
    });
    expect(describeTrim({ startMs: 1_000, endMs: 4_000 }, written)).toContain(
      "losslessly remuxed",
    );
  });
});

describe("describeCombine", () => {
  it("names the count, the new file and that the originals survive", () => {
    const message = describeCombine(2, clip({ path: "/c/combine-1756000000000.mp4" }));
    expect(message).toContain("Combined 2 clips");
    expect(message).toContain("combine-1756000000000.mp4");
    expect(message).toContain("in the library");
    expect(message).toContain("originals are untouched");
  });
});

describe("describeExport", () => {
  it("names the preset, the file, its size and where it is not", () => {
    const message = describeExport({
      preset: "gif",
      path: "/c/clip-1-gif.gif",
      size_bytes: 1_048_576,
      duration_ms: 1_000,
    });
    expect(message).toContain("GIF");
    expect(message).toContain("clip-1-gif.gif");
    expect(message).toContain("1.0 MiB");
    expect(message).toContain("not in the library");
  });

  it("falls back to the wire name for a preset this build does not know", () => {
    const message = describeExport({
      preset: "ultrawide",
      path: "/c/clip-1-ultrawide.mp4",
      size_bytes: 1,
      duration_ms: 1,
    });
    expect(message).toContain("ultrawide");
  });
});

describe("describePrune", () => {
  const outcome = (overrides: Partial<PruneOutcome> = {}): PruneOutcome => ({
    clips_removed: 0,
    sessions_removed: 0,
    thumbnails_removed: 0,
    sessions_refused: 0,
    rows_already_gone: 0,
    failed: 0,
    ...overrides,
  });

  it("says plainly that an empty reconcile had nothing to do", () => {
    const message = describePrune(outcome());
    expect(message).toContain("Nothing to remove");
    expect(message).not.toContain("failed");
  });

  it("names both libraries and that the disk was untouched", () => {
    const message = describePrune(outcome({ clips_removed: 12, sessions_removed: 1 }));
    expect(message).toContain("12 clips and 1 session");
    expect(message).toContain("already gone");
    expect(message).toContain("Nothing was deleted from disk");
  });

  it("uses the singular for one entry", () => {
    const message = describePrune(outcome({ clips_removed: 1 }));
    expect(message).toContain("1 clip");
    expect(message).toContain("its file was");
  });

  it("reports a session that started recording before the delete as left alone", () => {
    const message = describePrune(outcome({ clips_removed: 2, sessions_refused: 1 }));
    expect(message).toContain("still recording");
  });

  it("separates a failure from a successful removal", () => {
    const message = describePrune(outcome({ clips_removed: 2, failed: 1 }));
    expect(message).toContain("2 clips");
    expect(message).toContain("could not be removed");
  });

  it("reports an all-failed reconcile as a failure, not as nothing to do", () => {
    const message = describePrune(outcome({ failed: 3 }));
    expect(message).toContain("could not be deleted");
    expect(message).not.toContain("Nothing to remove");
  });
});

describe("a screenshot in the view model", () => {
  it("is marked as one, shows 'still' rather than a zero duration, and is its own picture", () => {
    const view = toClipView(
      clip({ kind: "screenshot", path: "/c/screenshot-1.png", duration_ms: 0, codec: "png" }),
      assetUrl,
    );
    expect(view.isScreenshot).toBe(true);
    expect(view.durationLabel).toBe("still");
    // The card uses `assetUrl` for a screenshot, so it must be the file's own URL.
    expect(view.assetUrl).toBe(assetUrl("/c/screenshot-1.png"));
  });

  it("leaves a clip a clip", () => {
    const view = toClipView(clip(), assetUrl);
    expect(view.isScreenshot).toBe(false);
    expect(view.durationLabel).not.toBe("still");
  });
});

describe("describeDelete", () => {
  const outcome = (overrides: Partial<DeleteOutcome> = {}): DeleteOutcome => ({
    id: 1,
    row_deleted: true,
    file_removed: true,
    orphaned_path: null,
    already_missing: false,
    bytes_reclaimed: 1_048_576,
    thumbnails_removed: 1,
    ...overrides,
  });

  it("reports a clean delete with what it freed", () => {
    expect(describeDelete(outcome(), "clip-1.mp4")).toBe(
      "clip-1.mp4 deleted, freeing 1.0 MiB.",
    );
  });

  it("says an id with no row deleted nothing at all", () => {
    const message = describeDelete(
      outcome({ row_deleted: false, file_removed: false }),
      "gone.mp4",
    );
    expect(message).toContain("no longer in the index");
    expect(message).toContain("nothing was deleted");
  });

  it("names the orphaned file when the unlink failed", () => {
    // Spec §8.2: this state is recoverable, but only if it is said out loud — reporting it
    // as a clean delete would leave a file nobody knows about filling the disk.
    const message = describeDelete(
      outcome({
        file_removed: false,
        orphaned_path: "/clips/stuck.mp4",
        bytes_reclaimed: 0,
      }),
      "stuck.mp4",
    );
    expect(message).toContain("/clips/stuck.mp4");
    expect(message).toContain("left on disk");
  });

  it("says when the file was already gone", () => {
    const message = describeDelete(
      outcome({ file_removed: false, already_missing: true }),
      "x.mp4",
    );
    expect(message).toContain("already gone");
  });
});

describe("filterClips", () => {
  const view = (over: Partial<ClipDto>) => toClipView(clip(over), assetUrl);
  const views = [
    view({
      id: 1,
      path: "/c/a.mp4",
      title: "Pentakill",
      game: "League of Legends",
      tags: ["ace", "funny"],
    }),
    view({ id: 2, path: "/c/b.mp4", game: "Valorant", tags: ["ace"] }),
    view({ id: 3, path: "/c/clutch-1v3.mp4", game: null, tags: [] }),
  ];
  const ids = (f: Partial<import("./clips").ClipFilter>) =>
    filterClips(views, { query: "", game: null, tag: null, ...f }).map(
      (v) => v.id,
    );

  it("shows everything with no filter, preserving order", () => {
    expect(ids({})).toEqual([1, 2, 3]);
  });

  it("searches title, file name, game and tags, case-insensitively, all words required", () => {
    expect(ids({ query: "PENTA" })).toEqual([1]);
    expect(ids({ query: "clutch" })).toEqual([3]);
    expect(ids({ query: "valorant" })).toEqual([2]);
    expect(ids({ query: "ace" })).toEqual([1, 2]);
    expect(ids({ query: "ace valorant" })).toEqual([2]);
    expect(ids({ query: "ace nothing" })).toEqual([]);
    expect(ids({ query: "   " })).toEqual([1, 2, 3]);
  });

  it("filters by exact game and tag, and combines with the query", () => {
    expect(ids({ game: "Valorant" })).toEqual([2]);
    expect(ids({ tag: "ace" })).toEqual([1, 2]);
    expect(
      ids({ tag: "ace", game: "League of Legends", query: "penta" }),
    ).toEqual([1]);
    expect(ids({ game: "valorant" })).toEqual([]);
  });

  it("lists games and tags with counts, ignoring clips that have none", () => {
    expect(gameOptions(views)).toEqual([
      { value: "League of Legends", count: 1 },
      { value: "Valorant", count: 1 },
    ]);
    expect(tagOptions(views)).toEqual([
      { value: "ace", count: 2 },
      { value: "funny", count: 1 },
    ]);
  });

  it("shows the title when there is one and the file name otherwise", () => {
    expect(views[0]?.displayName).toBe("Pentakill");
    expect(views[1]?.displayName).toBe("b.mp4");
  });
});

describe("parseTagInput", () => {
  it("splits on commas and newlines, normalizes and de-duplicates", () => {
    expect(parseTagInput(" Ace, clutch  play\nACE ,, ")).toEqual([
      "ace",
      "clutch play",
    ]);
    expect(parseTagInput("")).toEqual([]);
  });
});

describe("pruneThumbnails", () => {
  it("drops the pictures of clips that are no longer listed", () => {
    // A clip removed by the storage policy takes its cached picture with it, and nothing
    // else ever empties the map the window holds.
    expect(pruneThumbnails({ 1: "asset://1.jpg", 2: "asset://2.jpg" }, [1])).toEqual({
      1: "asset://1.jpg",
    });
    expect(pruneThumbnails({ 1: "asset://1.jpg" }, [])).toEqual({});
  });

  it("keeps the map as it is when every clip is still listed", () => {
    const thumbnails = { 1: "asset://1.jpg", 2: "asset://2.jpg" };
    expect(pruneThumbnails(thumbnails, [2, 1])).toBe(thumbnails);
  });
});

describe("shouldStartDrag", () => {
  it("needs the primary button held and movement past the click slop", () => {
    const down = { x: 10, y: 10 };
    expect(shouldStartDrag(down, { x: 12, y: 11 }, 1)).toBe(false);
    expect(shouldStartDrag(down, { x: 30, y: 10 }, 1)).toBe(true);
    expect(shouldStartDrag(down, { x: 30, y: 10 }, 0)).toBe(false);
    expect(shouldStartDrag(down, { x: 30, y: 10 }, 2)).toBe(false);
  });
});
