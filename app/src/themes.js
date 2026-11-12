import * as PIXI from 'pixi.js'
import { CRTFilter } from '@pixi/filter-crt'

export const ALL_THEMES = {}

export class Theme {
  constructor() {
  }

  static get(name) {
    return ALL_THEMES[name] || ALL_THEMES['default']
  }
}

// The original kube-ops-view look; the base class of all themes
class ClassicTheme {
  constructor() {
    this.name = 'classic'
    this.primaryColor = 0xaaaaff
    this.secondaryColor = 0x222233
    // style values for the drawing code; other themes change only what they need
    this.fontFamily = 'ShareTechMono'
    // flat: pods and nodes as thin bright outlines without fill; solid pool headers
    this.flat = false
    // node outline color in the flat style; null: pool color
    this.nodeBorderColor = null
    // text on the colored top handles of clusters and nodes
    this.handleTextColor = 0x000000
    this.podColors = { succeeded: 0xaaaaff, running: 0xaaffaa, pending: 0xffffaa, failed: 0xffaaaa }
    // null: use POOL_COLORS from node.js
    this.poolColors = null
    // null: the cluster box has no fill
    this.clusterFillColor = null
  }
  apply(stage) {
    stage.filters = []
  }
  register() {
    ALL_THEMES[this.name] = this
  }
}
new ClassicTheme().register()

// Flat design like the Windows Phone "Metro" tiles: black background, solid tiles, white text.
// Name "default": users without a saved choice and users with the saved choice "default" get it.
class FlatTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'default'
    this.primaryColor = 0xf0ff00 // yellow
    this.secondaryColor = 0x1f1f1f
    this.fontFamily = ['Segoe UI', 'Selawik', 'Open Sans', 'Noto Sans', 'Helvetica Neue', 'Arial', 'sans-serif']
    this.flat = true
    this.handleTextColor = 0xffffff
    this.nodeBorderColor = 0xf0ff00 // yellow
    // running: cyan; the other states keep their signal colors
    // this.podColors = { succeeded: 0x647687, running: 0x1ba1e2, pending: 0xf0a30a, failed: 0xff2a1a }
    this.poolColors = {
      'master': 0xd80073,
      'control-plane': 0xd80073,
      'infra': 0xfa6800,
      'infrastructure': 0xfa6800,
      'monitoring': 0xaa00ff,
      'worker': 0x00aba9,
      'compute': 0x00aba9,
      'default': 0x00aba9
    }
    this.clusterFillColor = 0x000000
  }
}
new FlatTheme().register()

// Flat theme variants in the Nokia Lumia colors; running pods are white
class FlatVariantTheme extends FlatTheme {
  constructor(name, clusterFillColor, nodeBorderColor, primaryColor) {
    super()
    this.name = name
    this.clusterFillColor = clusterFillColor
    this.nodeBorderColor = nodeBorderColor
    this.primaryColor = primaryColor
    this.podColors = { ...this.podColors, running: 0xffffff }
  }
}
// graphite background, cyan node borders, magenta accent
new FlatVariantTheme('lumia-cyan', 0x161616, 0x00b7c3, 0xe3008c).register()
// dark grey background, teal node borders, orange accent
new FlatVariantTheme('lumia-orange', 0x1b1b1b, 0x00aba9, 0xff8c00).register()
// dark blue background, lime node borders, Windows Phone blue accent
new FlatVariantTheme('lumia-green', 0x14213d, 0x7fba00, 0x0078d7).register()

class GreenTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'green'
    this.primaryColor = 0xaaffaa
    this.secondaryColor = 0x223322
  }
}
new GreenTheme().register()

class GreyTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'grey'
    this.primaryColor = 0xeeeeee
    this.secondaryColor = 0x333333
  }
}
new GreyTheme().register()

class BlackAndWhiteTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'blackandwhite'
    this.primaryColor = 0xffffff
    this.secondaryColor = 0x000000
  }
  apply(stage) {
    const filter = new PIXI.ColorMatrixFilter()
    filter.blackAndWhite()
    stage.filters = [filter]
  }
}
new BlackAndWhiteTheme().register()

class SepiaTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'sepia'
  }
  apply(stage) {
    const filter = new PIXI.ColorMatrixFilter()
    filter.sepia()
    stage.filters = [filter]
  }
}
new SepiaTheme().register()

class PolaroidTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'polaroid'
  }
  apply(stage) {
    const filter = new PIXI.ColorMatrixFilter()
    filter.polaroid()
    stage.filters = [filter]
  }
}
new PolaroidTheme().register()

class HighContrastTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'highcontrast'
    this.primaryColor = 0xffffff
    this.secondaryColor = 0x000000
  }
  apply(stage) {
    const filter = new PIXI.ColorMatrixFilter()
    filter.saturate(3)
    stage.filters = [filter]
  }
}
new HighContrastTheme().register()

class CRTTheme extends ClassicTheme {
  constructor() {
    super()
    this.name = 'crt'
    this.primaryColor = 0xaaaaff
    this.secondaryColor = 0x222233
  }
  apply(stage) {
    const filter = new CRTFilter({ time: 0.5 })

    stage.filters = [filter]
    this.filter = filter
    PIXI.Ticker.shared.add(this.animate, this)
  }

  animate(_delta) {
    this.filter.seed = Math.random()
    this.filter.time += 0.5
  }
}
new CRTTheme().register()
