import * as PIXI from 'pixi.js'

const FACTORS = {
  'n': 1 / 1000000000,
  'u': 1 / 1000000,
  'm': 1 / 1000,
  '': 1,
  'k': 1000,
  'M': Math.pow(1000, 2),
  'G': Math.pow(1000, 3),
  'T': Math.pow(1000, 4),
  'P': Math.pow(1000, 5),
  'E': Math.pow(1000, 6),
  'Ki': 1024,
  'Mi': Math.pow(1024, 2),
  'Gi': Math.pow(1024, 3),
  'Ti': Math.pow(1024, 4),
  'Pi': Math.pow(1024, 5),
  'Ei': Math.pow(1024, 6)
}

function hsvToRgb(h, s, v) {
  const cleanH = isNaN(h) || !isFinite(h) ? 0.4 : h
  const cleanS = isNaN(s) || !isFinite(s) ? 0.6 : s
  const cleanV = isNaN(v) || !isFinite(v) ? 1 : v
  let r, g, b
  const i = Math.floor(cleanH * 6)
  const f = cleanH * 6 - i
  const p = cleanV * (1 - cleanS)
  const q = cleanV * (1 - f * cleanS)
  const t = cleanV * (1 - (1 - f) * cleanS)
  switch (i % 6) {
    case 0:
      r = cleanV
      g = t
      b = p
      break
    case 1:
      r = q
      g = cleanV
      b = p
      break
    case 2:
      r = p
      g = cleanV
      b = t
      break
    case 3:
      r = p
      g = q
      b = cleanV
      break
    case 4:
      r = t
      g = p
      b = cleanV
      break
    case 5:
      r = cleanV
      g = p
      b = q
      break
    default:
      r = cleanV
      g = t
      b = p
      break
  }
  return new PIXI.Color([r, g, b]).toNumber()
}

function getBarColor(usage, capacity) {
  if (!capacity || capacity <= 0 || isNaN(capacity) || !isFinite(capacity)) {
    return hsvToRgb(0.4, 0.6, 1)
  }
  const u = isNaN(usage) || !isFinite(usage) || usage < 0 ? 0 : usage
  return hsvToRgb(Math.max(0, Math.min(1, 0.4 - (0.4 * (u / capacity)))), 0.6, 1)
}

function parseResource(v) {
  if (v === undefined || v === null) return 0
  if (typeof v === 'number') return isNaN(v) ? 0 : v
  const str = String(v).trim()
  if (!str) return 0
  const match = str.match(/^(\d*(?:\.\d+)?)\s*(\D*)$/)
  if (!match || !match[1]) return 0
  const factor = FACTORS[match[2]] || 1
  const parsed = parseFloat(match[1])
  return isNaN(parsed) ? 0 : parsed * factor
}

const metric = (metric, type) =>
  metric ? (metric[type] ? parseResource(metric[type]) : 0) : 0

const podResource = type => (containers, resource) =>
  (containers || [])
    .map(({ resources }) => resources ? metric(resources[resource], type) : 0)
    .reduce((a, b) => a + b, 0)

export { FACTORS, hsvToRgb, getBarColor, parseResource, metric, podResource }
