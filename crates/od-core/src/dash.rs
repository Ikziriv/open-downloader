//! DASH (`.mpd`) support for static VOD manifests: the chosen
//! `<Representation>` may list media via `<SegmentList>`,
//! `<SegmentTemplate>` (with `duration` or an explicit `<SegmentTimeline>`,
//! `$Number$`/`$Time$`/`$RepresentationID$`/`$Bandwidth$` expansion), or a
//! single `<BaseURL>`. Dynamic (live) manifests and unbounded timelines get
//! a clear `Error::Stream` instead of a broken file.

use std::path::PathBuf;

use reqwest::header::HeaderMap;
use roxmltree::Node;
use tokio::sync::mpsc;

use crate::engine::sidecar_paths;
use crate::stream::{concat_segments, fetch_segments, segments_dir};
use crate::{DownloadOptions, DownloadOutcome, DownloadSpec, Error, Event};

fn child<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve `BaseURL`s the way the spec intends: each level's value is
/// joined onto the URL inherited from the level above, outermost first.
fn rep_base_url(mpd_url: &str, rep: Node<'_, '_>) -> Result<reqwest::Url, Error> {
    let mut base = reqwest::Url::parse(mpd_url)
        .map_err(|e| Error::Stream(format!("bad MPD URL: {e}")))?;
    let mut chain = Vec::new();
    let mut cur = Some(rep);
    while let Some(n) = cur {
        chain.push(n);
        cur = n.parent();
    }
    for node in chain.iter().rev() {
        if let Some(b) = child_text(*node, "BaseURL") {
            base = base
                .join(&b)
                .map_err(|e| Error::Stream(format!("bad BaseURL {b:?}: {e}")))?;
        }
    }
    Ok(base)
}

/// Expand `$Name$` / `$Name%05d$` template tokens. `$$` escapes to `$`.
fn expand_template(tpl: &str, rep_id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::with_capacity(tpl.len() + 8);
    let mut rest = tpl;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push('$');
            rest = after;
            break;
        };
        let token = &after[..end];
        if token.is_empty() {
            out.push('$'); // $$ escape
        } else {
            let (name, fmt) = token.split_once('%').unwrap_or((token, ""));
            let width = fmt
                .trim_end_matches('d')
                .parse::<usize>()
                .unwrap_or(0);
            match name {
                "RepresentationID" => out.push_str(rep_id),
                "Bandwidth" => out.push_str(&bandwidth.to_string()),
                "Number" => out.push_str(&format!("{number:0width$}")),
                "Time" => out.push_str(&format!("{time:0width$}")),
                _ => {
                    out.push('$');
                    out.push_str(token);
                    out.push('$');
                }
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `PT1H2M3.5S` / `P…DT…` → seconds. Date components are ignored — period
/// durations in the wild only carry the time part.
fn iso_duration_secs(s: &str) -> Option<f64> {
    let s = s.strip_prefix('P')?.split('T').nth(1)?;
    let mut secs = 0.0;
    let mut num = String::new();
    for c in s.chars() {
        match c {
            'H' => {
                secs += num.parse::<f64>().ok()? * 3600.0;
                num.clear();
            }
            'M' => {
                secs += num.parse::<f64>().ok()? * 60.0;
                num.clear();
            }
            'S' => {
                secs += num.parse::<f64>().ok()?;
                num.clear();
            }
            c if c.is_ascii_digit() || c == '.' => num.push(c),
            _ => return None,
        }
    }
    if num.is_empty() { Some(secs) } else { None }
}

fn attr_u64(node: Node<'_, '_>, name: &str) -> Option<u64> {
    node.attribute(name)?.parse().ok()
}

/// The period's duration in timescale units: `Period@duration` wins over
/// `MPD@mediaPresentationDuration`.
fn period_duration_ts(rep: Node<'_, '_>, timescale: u64) -> Option<f64> {
    let mut node = Some(rep);
    while let Some(n) = node {
        if n.tag_name().name() == "Period"
            && let Some(secs) = n.attribute("duration").and_then(iso_duration_secs)
        {
            return Some(secs * timescale as f64);
        }
        if n.tag_name().name() == "MPD" {
            return n
                .attribute("mediaPresentationDuration")
                .and_then(iso_duration_secs)
                .map(|s| s * timescale as f64);
        }
        node = n.parent();
    }
    None
}

/// Expand a `<SegmentTemplate>` into (init, media) URLs.
fn template_urls(
    base: &reqwest::Url,
    rep: Node<'_, '_>,
    tpl: Node<'_, '_>,
) -> Result<Vec<String>, Error> {
    let resolve = |rel: &str| -> Result<String, Error> {
        Ok(base
            .join(rel)
            .map_err(|e| Error::Stream(format!("bad segment URL {rel:?}: {e}")))?
            .to_string())
    };
    let rep_id = rep.attribute("id").unwrap_or("");
    let bandwidth = attr_u64(rep, "bandwidth").unwrap_or(0);
    let timescale = attr_u64(tpl, "timescale").unwrap_or(1).max(1);
    let start = attr_u64(tpl, "startNumber").unwrap_or(1);
    let media = tpl
        .attribute("media")
        .ok_or_else(|| Error::Stream("SegmentTemplate without media".into()))?;

    // (number, time) for every media segment.
    let timeline = child(tpl, "SegmentTimeline");
    let mut entries: Vec<(u64, u64)> = Vec::new();
    if let Some(tl) = timeline {
        let mut n = start;
        let mut t = 0u64;
        for s in tl
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "S")
        {
            t = attr_u64(s, "t").unwrap_or(t);
            let d = attr_u64(s, "d")
                .ok_or_else(|| Error::Stream("SegmentTimeline S without d".into()))?;
            let r = s.attribute("r").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            let count = if r >= 0 {
                r as u64 + 1
            } else {
                // r=-1 repeats until the period ends — bounded by duration.
                let total = period_duration_ts(rep, timescale).ok_or_else(|| {
                    Error::Stream(
                        "open-ended SegmentTimeline needs a period duration".into(),
                    )
                })?;
                ((total - t as f64) / d as f64).ceil().max(0.0) as u64
            };
            for _ in 0..count {
                entries.push((n, t));
                n += 1;
                t += d;
            }
        }
    } else {
        let d = attr_u64(tpl, "duration").ok_or_else(|| {
            Error::Stream("SegmentTemplate needs duration or SegmentTimeline".into())
        })?;
        let total = period_duration_ts(rep, timescale).ok_or_else(|| {
            Error::Stream(
                "SegmentTemplate duration needs the period/mediaPresentationDuration".into(),
            )
        })?;
        let count = (total / d as f64).ceil() as u64;
        for i in 0..count {
            entries.push((start + i, i * d));
        }
    }
    if let Some(end) = attr_u64(tpl, "endNumber") {
        entries.retain(|(n, _)| *n <= end);
    }
    if entries.is_empty() {
        return Err(Error::Stream("SegmentTemplate produced no segments".into()));
    }

    let mut urls = Vec::with_capacity(entries.len() + 1);
    if let Some(init) = tpl.attribute("initialization") {
        urls.push(resolve(&expand_template(
            init,
            rep_id,
            bandwidth,
            start,
            entries[0].1,
        ))?);
    }
    for (n, t) in entries {
        urls.push(resolve(&expand_template(media, rep_id, bandwidth, n, t))?);
    }
    Ok(urls)
}

/// Absolute media URLs for the highest-bandwidth representation:
/// init segment first, then each media URL in document order.
fn segment_urls(mpd_url: &str, rep: Node<'_, '_>) -> Result<Vec<String>, Error> {
    let base = rep_base_url(mpd_url, rep)?;
    let resolve = |rel: &str| -> Result<String, Error> {
        Ok(base
            .join(rel)
            .map_err(|e| Error::Stream(format!("bad segment URL {rel:?}: {e}")))?
            .to_string())
    };

    // SegmentList/SegmentTemplate may be declared on the Representation or
    // inherited from its AdaptationSet.
    let inherited = |name: &str| {
        child(rep, name).or_else(|| rep.parent().and_then(|p| child(p, name)))
    };

    if let Some(list) = inherited("SegmentList") {
        let mut urls = Vec::new();
        if let Some(init) = child(list, "Initialization").and_then(|n| n.attribute("sourceURL")) {
            urls.push(resolve(init)?);
        }
        urls.extend(
            list.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "SegmentURL")
                .filter_map(|n| n.attribute("media"))
                .map(resolve)
                .collect::<Result<Vec<_>, _>>()?,
        );
        if urls.is_empty() {
            return Err(Error::Stream("DASH SegmentList had no SegmentURLs".into()));
        }
        return Ok(urls);
    }

    if let Some(tpl) = inherited("SegmentTemplate") {
        return template_urls(&base, rep, tpl);
    }

    // Whole-file representation: BaseURL *is* the media.
    if child(rep, "BaseURL").is_some() {
        return Ok(vec![base.to_string()]);
    }
    Err(Error::Stream(
        "unsupported DASH manifest (need SegmentList, SegmentTemplate, \
         or BaseURL; live isn't supported)"
            .into(),
    ))
}

/// Download a DASH stream to `spec.dest` (extension swapped to `.mp4` when
/// it was auto-derived as `.mpd`).
pub(crate) async fn download(
    client: &reqwest::Client,
    spec: &DownloadSpec,
    headers: &HeaderMap,
    opts: &DownloadOptions,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<DownloadOutcome, Error> {
    let text = client
        .get(&spec.url)
        .headers(headers.clone())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let doc = roxmltree::Document::parse(&text)
        .map_err(|e| Error::Stream(format!("bad MPD XML: {e}")))?;
    let mpd = doc.root_element();
    if mpd.tag_name().name() != "MPD" {
        return Err(Error::Stream("manifest isn't an MPD".into()));
    }
    if mpd.attribute("type") == Some("dynamic") {
        return Err(Error::Stream(
            "live DASH (type=\"dynamic\") isn't supported".into(),
        ));
    }

    // First Period only — later periods are other programs/ads.
    let scope: Vec<Node> = match child(mpd, "Period") {
        Some(p) => p.descendants().collect(),
        None => doc.descendants().collect(),
    };
    let rep = scope
        .iter()
        .filter(|n| n.is_element() && n.tag_name().name() == "Representation")
        .max_by_key(|n| {
            n.attribute("bandwidth")
                .and_then(|b| b.parse::<u64>().ok())
                .unwrap_or(0)
        })
        .ok_or_else(|| Error::Stream("MPD had no Representation".into()))?;
    let urls = segment_urls(&spec.url, *rep)?;

    let dest: PathBuf = if spec.dest.extension().is_some_and(|e| e == "mpd") {
        spec.dest.with_extension("mp4")
    } else {
        spec.dest.clone()
    };
    let (part, _) = sidecar_paths(&dest);
    let seg_dir = segments_dir(&dest);
    let paths = fetch_segments(client, &urls, headers, &seg_dir, opts, events, spec.id, None).await?;
    let bytes = concat_segments(&paths, &part).await?;
    let _ = tokio::fs::remove_dir_all(&seg_dir).await;
    let _ = events.send(Event::Progress {
        id: spec.id,
        downloaded: bytes,
        total: Some(bytes),
    });
    // Windows rename fails when the target exists — remove it first.
    let _ = tokio::fs::remove_file(&dest).await;
    tokio::fs::rename(&part, &dest).await?;
    Ok(DownloadOutcome {
        path: dest,
        resumed: false,
    })
}
