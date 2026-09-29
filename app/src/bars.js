import { FACTORS, getBarColor } from './utils.js'
import App from './app.js'
import * as PIXI from 'pixi.js'

export default class Bars extends PIXI.Graphics {
  constructor(entity, resources, tooltip) {
    super()
    this.entity = entity
    this.resources = resources
    this.tooltip = tooltip
  }

  draw() {
    const bars = this

    const barHeightPx = bars.entity.heightOfNodePx - (App.current.heightOfTopHandlePx + 5 + 3)
    const heightOfNodeWoPaddingPx = bars.entity.heightOfNodePx - 5

    // Background bar
    bars.rect(5, heightOfNodeWoPaddingPx - barHeightPx, 15, barHeightPx)
    bars.fill({ color: App.current.theme.primaryColor, alpha: 0.1 })
    bars.interactive = true

    // CPU
    const cpu = bars.resources && bars.resources.cpu
    if (cpu && cpu.capacity > 0) {
      const cpuHeight = barHeightPx / cpu.capacity
      const effectiveCpuCap = Math.max(0, cpu.capacity - (cpu.reserved || 0))

      // CPU requested bar
      const cpuReqTotal = (cpu.requested || 0) + (cpu.reserved || 0)
      const cpuReqHeight = Math.min(barHeightPx, cpuReqTotal * cpuHeight)
      if (cpuReqHeight > 0) {
        bars.rect(5, heightOfNodeWoPaddingPx - cpuReqHeight, 2.5, cpuReqHeight)
        bars.fill({ color: getBarColor(cpu.requested, effectiveCpuCap) })
      }

      // CPU used bar
      const cpuUsedHeight = Math.min(barHeightPx, (cpu.used || 0) * cpuHeight)
      if (cpuUsedHeight > 0) {
        bars.rect(7.5, heightOfNodeWoPaddingPx - cpuUsedHeight, 2.5, cpuUsedHeight)
        bars.fill({ color: getBarColor(cpu.used, cpu.capacity) })
      }

      // CPU reserved outline
      if (cpu.reserved > 0) {
        const cpuResHeight = Math.min(barHeightPx, cpu.reserved * cpuHeight)
        bars.rect(5, heightOfNodeWoPaddingPx - cpuResHeight, 5, cpuResHeight)
        bars.stroke({ width: 1, color: App.current.theme.primaryColor })
      }

      // CPU capacity grid
      for (let i = 0; i < cpu.capacity; i++) {
        bars.rect(5, heightOfNodeWoPaddingPx - (i + 1) * cpuHeight, 5, cpuHeight)
        bars.stroke({ width: 1, color: App.current.theme.primaryColor })
      }
    }

    // Memory
    const memory = bars.resources && bars.resources.memory
    if (memory && memory.capacity > 0) {
      const scale = memory.capacity / barHeightPx
      const effectiveMemCap = Math.max(0, memory.capacity - (memory.reserved || 0))

      // Memory requested bar
      const memReqTotal = (memory.requested || 0) + (memory.reserved || 0)
      const memReqHeight = Math.min(barHeightPx, memReqTotal / scale)
      if (memReqHeight > 0) {
        bars.rect(14, heightOfNodeWoPaddingPx - memReqHeight, 2.5, memReqHeight)
        bars.fill({ color: getBarColor(memory.requested, effectiveMemCap) })
      }

      // Memory used bar
      const memUsedHeight = Math.min(barHeightPx, (memory.used || 0) / scale)
      if (memUsedHeight > 0) {
        bars.rect(16.5, heightOfNodeWoPaddingPx - memUsedHeight, 2.5, memUsedHeight)
        bars.fill({ color: getBarColor(memory.used, memory.capacity) })
      }

      // Memory reserved outline
      if (memory.reserved > 0) {
        const memResHeight = Math.min(barHeightPx, memory.reserved / scale)
        bars.rect(14, heightOfNodeWoPaddingPx - memResHeight, 5, memResHeight)
        bars.stroke({ width: 1, color: App.current.theme.primaryColor })
      }

      // Memory capacity outline
      bars.rect(14, heightOfNodeWoPaddingPx - memory.capacity / scale, 5, memory.capacity / scale)
      bars.stroke({ width: 1, color: App.current.theme.primaryColor })
    }

    bars.on('mouseover', function () {
      let s = 'CPU: \n'
      const cpu = (bars.resources && bars.resources.cpu) || {}
      const cpuCap = cpu.capacity > 0 ? cpu.capacity : 'N/A'
      const cpuRes = typeof cpu.reserved === 'number' && cpu.capacity > 0 ? cpu.reserved.toFixed(2) : 'N/A'
      const cpuReq = typeof cpu.requested === 'number' && cpu.capacity > 0 ? cpu.requested.toFixed(2) : 'N/A'
      const cpuUsed = typeof cpu.used === 'number' && cpu.capacity > 0 ? cpu.used.toFixed(2) : 'N/A'
      s += '\t\t Capacity  : ' + cpuCap + '\n'
      s += '\t\t Reserved  : ' + cpuRes + '\n'
      s += '\t\t Requested : ' + cpuReq + '\n'
      s += '\t\t Used      : ' + cpuUsed + '\n'
      s += '\nMemory: \n'

      const mem = (bars.resources && bars.resources.memory) || {}
      const memCap = typeof mem.capacity === 'number' && mem.capacity > 0 ? (mem.capacity / FACTORS.Gi).toFixed(2) + ' GiB' : 'N/A'
      const memRes = typeof mem.reserved === 'number' && mem.capacity > 0 ? (mem.reserved / FACTORS.Gi).toFixed(2) + ' GiB' : 'N/A'
      const memReq = typeof mem.requested === 'number' && mem.capacity > 0 ? (mem.requested / FACTORS.Gi).toFixed(2) + ' GiB' : 'N/A'
      const memUsed = typeof mem.used === 'number' && mem.capacity > 0 ? (mem.used / FACTORS.Gi).toFixed(2) + ' GiB' : 'N/A'
      s += '\t\t Capacity  : ' + memCap + '\n'
      s += '\t\t Reserved  : ' + memRes + '\n'
      s += '\t\t Requested : ' + memReq + '\n'
      s += '\t\t Used      : ' + memUsed + '\n'

      s += '\nPods: \n'
      const pods = (bars.resources && bars.resources.pods) || {}
      const podsCap = typeof pods.capacity === 'number' && pods.capacity > 0 ? pods.capacity : 'N/A'
      const podsUsed = pods.used ?? 0
      s += '\t\t Capacity  : ' + podsCap + '\n'
      s += '\t\t Used      : ' + podsUsed + '\n'

      bars.tooltip.setText(s)
      bars.tooltip.position = bars.toGlobal(new PIXI.Point(22, 16))
      bars.tooltip.visible = true
    })
    bars.on('mouseout', function () {
      bars.tooltip.visible = false
    })

    return bars
  }

}
