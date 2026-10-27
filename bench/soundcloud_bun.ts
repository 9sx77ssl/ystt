#!/usr/bin/env bun
// Native SoundCloud AAC-HLS benchmark baseline. No yt-dlp, no ffmpeg, no browser.
// This mirrors the Rust client's public API and HLS assembly algorithm.

const UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'

async function json(url: string): Promise<any> {
  const response = await fetch(url, { headers: { 'user-agent': UA, accept: 'application/json' } })
  if (!response.ok) throw new Error(`HTTP ${response.status} ${url}`)
  return response.json()
}

async function clientId(): Promise<string> {
  const html = await (await fetch('https://soundcloud.com/', { headers: { 'user-agent': UA } })).text()
  const hydrated = html.match(/"hydratable"\s*:\s*"apiClient"\s*,\s*"data"\s*:\s*\{\s*"id"\s*:\s*"([^"]+)"/)?.[1]
  if (hydrated) return hydrated
  for (const match of html.matchAll(/<script[^>]+src="([^"]+)"/g)) {
    const src = match[1]!
    if (!src.startsWith('https://a-v2.sndcdn.com/')) continue
    const body = await (await fetch(src, { headers: { 'user-agent': UA } })).text()
    const id = body.match(/client_id\s*:\s*"([A-Za-z0-9_-]{16,128})"/)?.[1]
    if (id) return id
  }
  throw new Error('no client_id')
}

async function main() {
  const input = process.argv[2] ?? 'https://soundcloud.com/treezymonster812/rozovoe-vino-2-ft-morgenshtern'
  const output = process.argv[3] ?? '/tmp/opencode/bun-sc.m4a'
  const started = performance.now()
  const cid = await clientId()
  const resolveUrl = `https://api-v2.soundcloud.com/resolve?url=${encodeURIComponent(input)}&client_id=${cid}`
  const track = await json(resolveUrl)
  const transcodings = (track.media?.transcodings ?? []).filter(
    (t: any) => !t.snipped && (t.preset === 'aac_160k' || t.preset === 'aac_96k' || t.preset === 'mp3_1_0'),
  )
  transcodings.sort((a: any, b: any) => {
    const rank = (p: string) => (p === 'aac_160k' ? 0 : p === 'mp3_1_0' ? 1 : 2)
    return rank(a.preset) - rank(b.preset)
  })
  if (!transcodings[0]) throw new Error('no compatible stream')
  const streamUrl = new URL(transcodings[0].url)
  streamUrl.searchParams.set('client_id', cid)
  if (track.track_authorization) streamUrl.searchParams.set('track_authorization', track.track_authorization)
  const stream = await json(streamUrl.toString())
  const playlistResponse = await fetch(stream.url, { headers: { 'user-agent': UA } })
  if (!playlistResponse.ok) throw new Error(`HLS HTTP ${playlistResponse.status}`)
  const playlistUrl = new URL(stream.url)
  const text = await playlistResponse.text()
  const lines = text.split('\n').map((line) => line.trim()).filter(Boolean)
  let init: string | null = null
  let mediaUrl = playlistUrl.toString()
  const nested = lines.find((line, index) => lines[index - 1]?.startsWith('#EXT-X-STREAM-INF') && !line.startsWith('#'))
  if (nested) {
    mediaUrl = new URL(nested, playlistUrl).toString()
    const nestedResponse = await fetch(mediaUrl, { headers: { 'user-agent': UA } })
    const nestedText = await nestedResponse.text()
    return assemble(nestedText, new URL(mediaUrl), output, started, track)
  }
  return assemble(text, playlistUrl, output, started, track)
}

async function assemble(text: string, base: URL, output: string, started: number, track: any) {
  const lines = text.split('\n').map((line) => line.trim()).filter(Boolean)
  let init: string | null = null
  const segments: string[] = []
  for (const line of lines) {
    if (line.startsWith('#EXT-X-MAP')) {
      const uri = line.match(/URI="([^"]+)"/)?.[1]
      if (uri) init = new URL(uri, base).toString()
    } else if (line.startsWith('#EXT-X-KEY')) {
      if (!line.includes('METHOD=NONE')) throw new Error('encrypted HLS')
    } else if (!line.startsWith('#')) {
      segments.push(new URL(line, base).toString())
    }
  }
  const parts: Uint8Array[] = []
  if (init) parts.push(new Uint8Array(await (await fetch(init)).arrayBuffer()))
  const batch = 8
  for (let i = 0; i < segments.length; i += batch) {
    const group = segments.slice(i, i + batch)
    const buffers = await Promise.all(
      group.map(async (segment) => new Uint8Array(await (await fetch(segment)).arrayBuffer())),
    )
    parts.push(...buffers)
  }
  let total = 0
  for (const part of parts) total += part.length
  const outputBytes = new Uint8Array(total)
  let offset = 0
  for (const part of parts) {
    outputBytes.set(part, offset)
    offset += part.length
  }
  await Bun.write(output, outputBytes)
  console.log(
    JSON.stringify({
      provider: 'soundcloud',
      runtime: 'bun',
      track_id: String(track.id),
      title: track.title,
      bytes: total,
      segments: segments.length,
      init: Boolean(init),
      ms: performance.now() - started,
    }),
  )
}

main().catch((error) => {
  console.error(error)
  process.exit(1)
})
