import { test } from "node:test";
import assert from "node:assert/strict";
import { MAX_LINKS, cookieHeader, fullSizeImages, isMagnet, largestSrcset, linkFilter, linkKind, linksInText, normalizeLinks, siteOf, textMatcher } from "../lib/links.js";

test("a link's chip is the extension of its path, whatever the query says", () => {
  assert.equal(linkKind("https://a.example/v/Clip.MP4?x=1.zip"), "Video");
  assert.equal(linkKind("https://a.example/song.flac"), "Audio");
  assert.equal(linkKind("https://a.example/pack.tar.gz"), "Archives");
  assert.equal(linkKind("https://a.example/setup.exe#top"), "Programs");
  assert.equal(linkKind("https://a.example/paper.pdf"), "Documents");
  assert.equal(linkKind("https://a.example/a.webp"), "Images");
  for (const none of ["https://a.example/", "https://a.example/page", "https://a.example/index.html", "not a url"]) {
    assert.equal(linkKind(none), "", none);
  }
});

test("the largest srcset image wins, resolved against the page", () => {
  const base = "https://a.example/gallery/";
  assert.equal(largestSrcset("small.jpg 320w, big.jpg 1280w, mid.jpg 640w", base), "https://a.example/gallery/big.jpg");
  assert.equal(largestSrcset("a.jpg, b.jpg 2x", base), "https://a.example/gallery/b.jpg");
  assert.equal(largestSrcset("a.jpg 1x,/b.jpg 3x", base), "https://a.example/b.jpg");
  assert.equal(largestSrcset("https://cdn.example/w_100,h_100/x.jpg 1x, https://cdn.example/w_200,h_200/x.jpg 2x", base), "https://cdn.example/w_200,h_200/x.jpg");
  assert.equal(largestSrcset("", base), "");
  assert.equal(largestSrcset(undefined, base), "");
});

test("links are absolute http(s), once each, without fragments, and capped", () => {
  const links = normalizeLinks([
    "https://a.example/x.zip#part",
    "https://a.example/x.zip",
    "HTTP://B.Example/y.pdf",
    "javascript:void(0)",
    "mailto:a@b.example",
    "blob:https://a.example/123",
    "data:text/plain,hi",
    "/relative",
    "",
  ]);
  assert.deepEqual(links, [
    { url: "https://a.example/x.zip", host: "a.example", kind: "Archives" },
    { url: "http://b.example/y.pdf", host: "b.example", kind: "Documents" },
  ]);
  const many = Array.from({ length: MAX_LINKS + 10 }, (_, n) => `https://a.example/${n}`);
  assert.equal(normalizeLinks(many).length, MAX_LINKS);
});

test("same site is the registrable domain, roughly", () => {
  assert.equal(siteOf("cdn.files.example.com"), "example.com");
  assert.equal(siteOf("example.com"), "example.com");
  assert.equal(siteOf("news.bbc.co.uk"), "bbc.co.uk");
  assert.equal(siteOf("shop.example.de"), "example.de");
  assert.equal(siteOf("192.168.1.20"), "192.168.1.20");
  assert.equal(siteOf("WWW.Example.com."), "example.com");
});

test("the text box takes plain text or a /regex/", () => {
  assert.ok(textMatcher("ZIP")("https://a.example/x.zip"));
  assert.ok(!textMatcher("rar")("https://a.example/x.zip"));
  assert.ok(textMatcher("")("https://a.example/x.zip"));
  assert.ok(textMatcher("/part\\d+\\.rar$/")("https://a.example/f.PART2.rar"));
  assert.ok(!textMatcher("/^http:/")("https://a.example/"));
  assert.equal(textMatcher("/([/"), null);
});

test("filters combine chips, text and same site", () => {
  const links = normalizeLinks([
    "https://a.example/x.zip",
    "https://cdn.a.example/v.mp4",
    "https://b.example/y.zip",
    "https://a.example/page",
  ]);
  const urls = (filters) => links.filter(linkFilter({ pageHost: "www.a.example", ...filters })).map((link) => link.url);
  assert.equal(urls({}).length, 4, "no chip chosen shows all");
  assert.deepEqual(urls({ kinds: ["Archives"] }), ["https://a.example/x.zip", "https://b.example/y.zip"]);
  assert.deepEqual(urls({ kinds: ["Archives", "Video"], sameSite: true }), ["https://a.example/x.zip", "https://cdn.a.example/v.mp4"]);
  assert.deepEqual(urls({ query: "y." }), ["https://b.example/y.zip"]);
  assert.deepEqual(urls({ query: "/([/" }), [], "a broken regex matches nothing");
});

test("a Cookie header leaves out what it can't carry", () => {
  const cookies = [
    { name: "sid", value: "a b" },
    { name: "bad", value: "x;y" },
    { name: "", value: "nameless" },
    { name: "nl", value: "a\nb" },
    { name: "t", value: "" },
  ];
  assert.equal(cookieHeader(cookies), "sid=a b; t=");
  assert.equal(cookieHeader([]), "");
  assert.equal(cookieHeader(undefined), "");
});

test("magnet links and .torrent files are listed as Torrents", () => {
  const magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=ubuntu.iso";
  assert.equal(linkKind(magnet), "Torrents");
  assert.equal(linkKind("https://a.example/ubuntu.iso.torrent"), "Torrents");
  assert.deepEqual(normalizeLinks([magnet, magnet, "magnet:no-query"]), [{ url: magnet, host: "", kind: "Torrents" }]);
  assert.equal(isMagnet(magnet), true);
  assert.equal(isMagnet("https://a.example/?magnet:?x"), false);
});

test("the links written in a selection, without the punctuation after them", () => {
  const text = "Get it at https://a.example/x.zip, or (https://b.example/y.iso). Torrent: magnet:?xt=urn:btih:abc&dn=y; again https://a.example/x.zip!";
  assert.deepEqual(linksInText(text), ["https://a.example/x.zip", "https://b.example/y.iso", "magnet:?xt=urn:btih:abc&dn=y"]);
  assert.deepEqual(linksInText("no links, ftp://x.example/y nor javascript:alert(1)"), []);
  assert.deepEqual(linksInText(undefined), []);
});

test("full-size images: a thumbnail's linked original, else the image when big enough", () => {
  const images = [
    { url: "https://a.example/thumb1.jpg", width: 150, height: 100, linked: "https://a.example/full1.jpg" },
    { url: "https://a.example/thumb2.jpg", width: 150, height: 100, linked: "https://a.example/post/2" },
    { url: "https://a.example/hero.webp", width: 1920, height: 1080, linked: "" },
    { url: "https://a.example/hero.webp", width: 1920, height: 1080 },
  ];
  assert.deepEqual(fullSizeImages(images), ["https://a.example/full1.jpg", "https://a.example/thumb2.jpg", "https://a.example/hero.webp"]);
  assert.deepEqual(fullSizeImages(images, 800), ["https://a.example/full1.jpg", "https://a.example/hero.webp"], "a linked original's size is not known");
});
