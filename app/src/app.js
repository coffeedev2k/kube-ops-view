import Tooltip from './tooltip.js'
import Cluster from './cluster.js'
import { getNodePoolType } from './node.js'
import { Pod, ALL_PODS, ALL_SORTS, ALL_STATUS_FILTERS } from './pod.js'
import SelectBox from './selectbox.js'
import { Theme, ALL_THEMES } from './themes.js'
import { DESATURATION_FILTER } from './filters.js'
import { JSON_delta } from './vendor/json_delta.js'
import Config from './config.js'
import addWheelListener from './vendor/addWheelListener.js'
import * as PIXI from 'pixi.js'


export default class App {

  constructor(config) {
    const params = this.parseLocationHash()

    this.config = Config.fromParams(params)
    this.config.nodeLinkUrlTemplate = config['node_link_url_template']
    this.config.podLinkUrlTemplate = config['pod_link_url_template']
    this.config.route_prefix = config['route_prefix']

    this.filterString = params.get('q') || ''
    this.selectedClusters = new Set((params.get('clusters') || '').split(',').filter(x => x))
    // keys "<cluster>/<namespace>/<name>" of pods in the cluster data (drawn or not)
    this.seenPods = new Set()
    // pod key -> node name (null: unassigned) from the previous update
    this.podNodeNames = new Map()
    // clusters that had data in the previous update
    this.knownClusterIds = new Set()

    // check localStorage, use the first function as a default option
    const indexSorterFn = ALL_SORTS.findIndex(obj => obj.text === (localStorage.getItem('sorterFn') || ALL_SORTS[0].text))
    this.sorterFn = ALL_SORTS[indexSorterFn].value

    // filterFn
    const indexStatusFilterFn = ALL_STATUS_FILTERS.findIndex(obj => obj.text === (localStorage.getItem('statusFilterFn') || ALL_STATUS_FILTERS[0].text))
    this.statusFilterFn = ALL_STATUS_FILTERS[indexStatusFilterFn].value

    this.theme = Theme.get(localStorage.getItem('theme'))

    // collapsed pools filter
    const collapsedParam = params.get('collapsed')
    if (collapsedParam !== null && collapsedParam !== undefined) {
      this.collapsedPools = new Set(collapsedParam.split(',').filter(x => x))
    } else {
      const stored = localStorage.getItem('collapsedPools')
      if (stored) {
        try {
          this.collapsedPools = new Set(JSON.parse(stored))
        } catch {
          this.collapsedPools = new Set()
        }
      } else {
        this.collapsedPools = new Set()
      }
    }

    this.eventSource = null
    this.connectTime = null
    this.keepAliveTimer = null
    this.clusters = new Map()
    // cluster id -> number that changes with each data change: update() redraws only changed clusters
    this.clusterDataVersions = new Map()
    // changes when the layout of all clusters changes (pool collapse, sorting)
    this.layoutVersion = 0
    this.clusterStatuses = new Map()
    this.viewContainerTargetPosition = new PIXI.Point()
    this.bootstrapping = true

    this.startDrawingPodsAt = 24
    this.defaultPodsPerRow = 6
    this.defaultWidthOfNodePx = 105
    this.defaultHeightOfNodePx = 115
    this.sizeOfPodPx = 13
    this.heightOfTopHandlePx = 15
  }

  decodeHashValue(raw) {
    // Older builds re-encoded every value on each update ('%' -> '%25' -> '%2525' ...),
    // so collapse the nested '%25' layers before the single decode.
    const collapsed = raw.replace(/%25(?:25)*/gi, '%')
    for (const candidate of [collapsed, raw]) {
      try {
        return decodeURIComponent(candidate)
      } catch {
        // malformed escape sequence, try the next candidate
      }
    }
    return raw
  }

  parseLocationHash() {
    // hash startswith #; returns decoded values
    const hash = document.location.hash.substring(1)
    const params = new Map()
    for (const pair of hash.split(';')) {
      const separatorIndex = pair.indexOf('=')
      if (separatorIndex > 0) {
        params.set(pair.substring(0, separatorIndex), this.decodeHashValue(pair.substring(separatorIndex + 1)))
      }
    }
    return params
  }

  changeLocationHash(key, value) {
    const params = this.parseLocationHash()
    params.set(key, value)
    const pairs = []
    for (const [key, value] of params) {
      if (value) {
        pairs.push(key + '=' + encodeURIComponent(value))
      }
    }

    const hash = '#' + pairs.sort().join(';')
    if (hash === document.location.hash || (hash === '#' && !document.location.hash)) {
      return
    }
    // replaceState: view state changes on every update, do not add browser history entries
    window.history.replaceState(window.history.state, '', hash)
  }

  nameMatches(pod, searchString) {
    const name = pod.name
    return name && name.includes(searchString)
  }

  labelMatches(pod, name, value) {
    const labels = pod.labels
    return labels && labels[name] === value
  }

  namespaceMatches(pod, value) {
    return pod.namespace === value
  }

  createMatchesFunctionForQuery(query) {
    if (query.startsWith('namespace=')) {
      // filter by namespace
      const value = query.split('namespace=', 2)[1]
      return pod => this.namespaceMatches(pod, value)
    } else if (query.includes('=')) {
      // filter by label
      const [label, value] = query.split('=', 2)
      return pod => this.labelMatches(pod, label, value)
    }
    else {
      // filter by name
      return pod => this.nameMatches(pod, query)
    }
  }


  filter() {
    const searchString = this.filterString
    if (this.searchText) {
      // this.searchText might be undefined (dashboard mode)
      this.searchText.text = searchString
    }
    this.changeLocationHash('q', searchString)
    const elementDisplayFilter = DESATURATION_FILTER
    const filterableElements = []
    const matchesQuery = this.createMatchesFunctionForQuery(searchString)
    for (const cluster of this.viewContainer.children) {
      for (const node of cluster.children) {
        if (node.pod) { // node is actually unassigned pod
          filterableElements.push(node)
        }
        for (const pod of node.children) {
          if (pod.pod) {
            filterableElements.push(pod)
          }
        }
      }
    }

    filterableElements.forEach(value => {
      if (!matchesQuery(value.pod)) {
        value.filters = [elementDisplayFilter]
      } else {
        // TODO: pod might have other filters set..
        value.filters = []
      }
    })

    filterableElements.forEach(value => {
      if (!this.statusFilterFn(value.pod)) {
        value.filters = [elementDisplayFilter]
      }
    })
  }

  async initialize() {
     App.current = this
    window.kubeOpsView = this // debug: theme changes from the DevTools console


    // create the renderer (async in pixi.js 8.x)
    const noWebGL = this.config.renderer === 'canvas'
    const renderer = await PIXI.autoDetectRenderer({
      width: window.innerWidth,
      height: window.innerHeight,
      resolution: 2,
      autoDensity: true,
      preference: noWebGL ? 'webgl' : undefined
    })
    renderer.canvas.style.display = 'block'

    window.onresize = function () {
      renderer.resize(window.innerWidth, window.innerHeight)
    }

    //Add the canvas to the HTML document
    document.body.appendChild(renderer.canvas)
    this.renderer = renderer

    //Create a container object called the `stage`
    this.stage = new PIXI.Container()

    this.registerEventListeners()
    setInterval(this.pruneUnavailableClusters.bind(this), 5 * 1000)

    if (this.config.reloadIntervalSeconds) {
      setTimeout(function () {
        location.reload(false)
      }, this.config.reloadIntervalSeconds * 1000)
    }
  }

  registerEventListeners() {
    function downHandler(event) {
      const panAmount = 20
      if (event.key == 'ArrowLeft') {
        this.viewContainerTargetPosition.x += panAmount
      }
      else if (event.key == 'ArrowRight') {
        this.viewContainerTargetPosition.x -= panAmount
      }
      if (event.key == 'ArrowUp') {
        this.viewContainerTargetPosition.y += panAmount
      }
      else if (event.key == 'ArrowDown') {
        this.viewContainerTargetPosition.y -= panAmount
      }
      if (event.key == 'PageUp') {
        this.viewContainerTargetPosition.y += window.innerHeight
      }
      else if (event.key == 'PageDown') {
        this.viewContainerTargetPosition.y -= window.innerHeight
      }
      else if (event.key == 'Home') {
        this.viewContainerTargetPosition.x = 20
        this.viewContainerTargetPosition.y = this.config.dashboardMode ? 20 : 40
      }
      else if (event.key && event.key.length == 1 && !event.ctrlKey && !event.metaKey) {
        this.filterString += event.key
        this.filter()
        event.preventDefault()
      }
      else if (event.key == 'Backspace') {
        this.filterString = this.filterString.slice(0, Math.max(0, this.filterString.length - 1))
        this.filter()
        event.preventDefault()
      }
    }

    var isDragging = false,
      prevX, prevY

    function mouseDownHandler(event) {
      if (event.button == 0 || event.button == 1) {
        prevX = event.clientX
        prevY = event.clientY
        isDragging = true
        this.renderer.canvas.style.cursor = 'move'
      }
    }

    function mouseMoveHandler(event) {
      if (!isDragging) {
        return
      }
      var dx = event.clientX - prevX
      var dy = event.clientY - prevY

      this.viewContainer.x += dx
      this.viewContainer.y += dy
      // stop any current move animation
      this.viewContainerTargetPosition.x = this.viewContainer.x
      this.viewContainerTargetPosition.y = this.viewContainer.y
      prevX = event.clientX
      prevY = event.clientY
    }

    function mouseUpHandler(_event) {
      isDragging = false
      this.renderer.canvas.style.cursor = 'default'
    }

    function touchStartHandler(event) {
      if (event.touches.length == 1) {
        const touch = event.touches[0]
        prevX = touch.clientX
        prevY = touch.clientY
        isDragging = true
      }
    }

    function touchMoveHandler(event) {
      if (!isDragging) {
        return
      }
      if (event.touches.length == 1) {
        const touch = event.touches[0]
        var dx = touch.clientX - prevX
        var dy = touch.clientY - prevY

        this.viewContainer.x += dx
        this.viewContainer.y += dy
        // stop any current move animation
        this.viewContainerTargetPosition.x = this.viewContainer.x
        this.viewContainerTargetPosition.y = this.viewContainer.y
        prevX = touch.clientX
        prevY = touch.clientY
      }
    }

    function touchEndHandler(_event) {
      isDragging = false
    }

    addEventListener('keydown', downHandler.bind(this), false)
    addEventListener('mousedown', mouseDownHandler.bind(this), false)
    addEventListener('mousemove', mouseMoveHandler.bind(this), false)
    addEventListener('mouseup', mouseUpHandler.bind(this), false)
    addEventListener('touchstart', touchStartHandler.bind(this), false)
    addEventListener('touchmove', touchMoveHandler.bind(this), false)
    addEventListener('touchend', touchEndHandler.bind(this), false)

    const that = this

    const minScale = 1 / 32
    const maxScale = 32

    function zoom(x, y, isZoomIn) {
      const view = that.viewContainer
      const direction = isZoomIn ? 1 : -1
      const factor = (1 + direction * 0.1)
      const oldScale = view.scale.x
      const newScale = Math.min(Math.max(oldScale * factor, minScale), maxScale)
      if (newScale === oldScale) {
        return
      }
      // zoom around the mouse pointer: the view point under the pointer stays at the same screen position
      // (viewContainer is a direct child of the unscaled stage: screen = position + local * scale)
      const localX = (x - view.x) / oldScale
      const localY = (y - view.y) / oldScale
      view.scale.set(newScale)
      view.x = x - localX * newScale
      view.y = y - localY * newScale

      // stop any current move animation
      that.viewContainerTargetPosition.x = view.x
      that.viewContainerTargetPosition.y = view.y
    }

    addWheelListener(this.renderer.canvas, function (e) {
      zoom(e.clientX, e.clientY, e.deltaY < 0)
    })
  }

  drawMenuBar() {
    const menuBar = new PIXI.Graphics()
    menuBar.allowChildren = true
    menuBar.rect(0, 0, this.renderer.width, 28)
    menuBar.fill({ color: this.theme.secondaryColor })
    menuBar.moveTo(0, 28)
    menuBar.lineTo(this.renderer.width, 28)
    menuBar.stroke({ width: 2, color: this.theme.secondaryColor })
    menuBar.rect(20, 3, 200, 22)
    menuBar.stroke({ width: 1, color: this.theme.primaryColor })
    this.stage.addChild(menuBar)

    const searchPrompt = new PIXI.Text({
      text: '>',
      style: { fontFamily: this.theme.fontFamily, fontSize: 14, fill: this.theme.primaryColor }
    })
    searchPrompt.x = 26
    searchPrompt.y = 8
    PIXI.Ticker.shared.add(function (_) {
      var v = Math.sin((PIXI.Ticker.shared.lastTime % 2000) / 2000. * Math.PI)
      searchPrompt.alpha = v
    })
    this.stage.addChild(searchPrompt)

    const searchText = new PIXI.Text({ text: '', style: { fontFamily: this.theme.fontFamily, fontSize: 14, fill: this.theme.primaryColor } })
    searchText.x = 40
    searchText.y = 8
    this.stage.addChild(searchText)

    const app = this
    const selectBox = new SelectBox(ALL_SORTS, this.sorterFn, function (text, value) {
      app.changeSorting(text, value)
    })
    selectBox.x = 265
    selectBox.y = 3
    menuBar.addChild(selectBox.draw())

    const themeOptions = Object.keys(ALL_THEMES).sort().map(name => {
      return { text: name.toUpperCase(), value: name }
    })
    const themeSelector = new SelectBox(themeOptions, this.theme.name, function (text, value) {
      app.switchTheme(text, value)
    })
    themeSelector.x = 420
    themeSelector.y = 3
    menuBar.addChild(themeSelector.draw())


    const statusFilterBox = new SelectBox(ALL_STATUS_FILTERS, this.statusFilterFn, function (text, value) {
      app.changeStatusFilter(text, value)
    })
    statusFilterBox.x = 585
    statusFilterBox.y = 3
    menuBar.addChild(statusFilterBox.draw())

    this.searchText = searchText
  }

  draw() {
    if (this.viewContainer) {
      // theme switch: the old cluster components are not used again
      for (const component of this.viewContainer.removeChildren()) {
        component.destroy()
      }
    }
    this.stage.removeChildren()
    this.theme.apply(this.stage)

    const viewContainer = new PIXI.Container()
    viewContainer.scale.set(this.config.initialScale)
    viewContainer.x = 20
    viewContainer.y = this.config.dashboardMode ? 20 : 40
    this.viewContainerTargetPosition.x = viewContainer.x
    this.viewContainerTargetPosition.y = viewContainer.y
    this.stage.addChild(viewContainer)

    if (!this.config.dashboardMode) {
      this.drawMenuBar()
    }

    const tooltip = this.tooltip || new Tooltip()
    tooltip.draw()
    this.stage.addChild(tooltip)

    this.viewContainer = viewContainer
    this.tooltip = tooltip
  }

  animatePodCreation(originalPod, globalPosition) {
    const pod = new Pod(originalPod.pod, null, this.tooltip)
    pod.draw()
    pod.blendMode = 'add'
    pod.interactive = false
    const targetPosition = globalPosition
    const angle = Math.random() * Math.PI * 2
    const cos = Math.cos(angle)
    const sin = Math.sin(angle)
    const distance = Math.max(200, Math.random() * Math.min(this.renderer.width, this.renderer.height))
    // blur filter looks cool, but has huge performance penalty
    // const blur = new PIXI.BlurFilter(20, 2)
    // pod.filters = [blur]
    pod.pivot.x = pod.width / 2
    pod.pivot.y = pod.height / 2
    pod.alpha = 0
    pod._progress = 0
    originalPod.visible = false
    const that = this
    const tick = function (ticker) {
      // progress goes from 0 to 1
      const progress = Math.min(1, pod._progress + (0.01 * ticker.deltaTime))
      const scale = 1 + ((1 - progress) * 140)
      pod._progress = progress
      pod.x = targetPosition.x + (distance * cos * (1 - progress))
      pod.y = targetPosition.y + (distance * sin * (1 - progress))
      pod.alpha = progress
      pod.rotation = progress * progress * Math.PI * 2
      // blur.blur = (1 - alpha) * 20
      pod.scale.set(scale)
      if (progress >= 1) {
        PIXI.Ticker.shared.remove(tick)
        that.stage.removeChild(pod)
        pod.destroy()
        originalPod.visible = true
      }
    }
    PIXI.Ticker.shared.add(tick)
    this.stage.addChild(pod)
  }

  animatePodDeletion(originalPod, globalPosition) {
    const pod = new Pod(originalPod.pod, null, this.tooltip)
    pod.draw()
    pod.blendMode = 'add'
    const globalCenter = new PIXI.Point(globalPosition.x + pod.width / 2, globalPosition.y + pod.height / 2)
    const blur = new PIXI.BlurFilter({ strength: 4 })
    pod.filters = [blur]
    pod.position = globalPosition.clone()
    pod.alpha = 1
    pod._progress = 1
    originalPod.destroy()
    const that = this
    const tick = function (ticker) {
      // progress goes from 1 to 0
      const progress = Math.max(0, pod._progress - (0.02 * ticker.deltaTime))
      const scale = 1 + ((1 - progress) * 8)
      pod._progress = progress
      pod.alpha = progress
      pod.scale.set(scale)
      pod.position.set(globalCenter.x - pod.width / 2, globalCenter.y - pod.height / 2)

      if (progress <= 0) {
        PIXI.Ticker.shared.remove(tick)
        that.stage.removeChild(pod)
        pod.destroy()
      }
    }
    PIXI.Ticker.shared.add(tick)
    this.stage.addChild(pod)
  }

  /**
   * Location of each pod in the cluster data.
   * @returns {Map} - pod key "<cluster>/<namespace>/<name>" -> { clusterId, node (null: unassigned), pod }
   */
  collectPodLocations(clusters) {
    const locations = new Map()
    for (const cluster of clusters) {
      if (!cluster) continue
      for (const node of Object.values(cluster.nodes || {})) {
        for (const pod of Object.values((node && node.pods) || {})) {
          if (pod && pod.namespace && pod.name) {
            locations.set(cluster.id + '/' + pod.namespace + '/' + pod.name, { clusterId: cluster.id, node, pod })
          }
        }
      }
      for (const pod of Object.values(cluster.unassigned_pods || {})) {
        if (pod && pod.namespace && pod.name) {
          locations.set(cluster.id + '/' + pod.namespace + '/' + pod.name, { clusterId: cluster.id, node: null, pod })
        }
      }
    }
    return locations
  }

  /**
   * Expand the collapsed pool of each node that got a pod scheduled since the previous update
   * (a new pod on the node, or a pending pod that got the node), to show where the pod was deployed.
   * Not for the first data of a cluster (bootstrap, newly shown cluster): all its pods are "new" then.
   */
  expandPoolsOfScheduledPods(podLocations) {
    let changed = false
    for (const [key, location] of podLocations) {
      if (!location.node || this.podNodeNames.get(key)) {
        // unassigned, or it was on a node already
        continue
      }
      if (!this.knownClusterIds.has(location.clusterId)) {
        continue
      }
      if (this.selectedClusters.size && !this.selectedClusters.has(location.clusterId)) {
        continue
      }
      const poolType = getNodePoolType(location.node.labels || {})
      if (this.isPoolCollapsed(location.clusterId, poolType)) {
        console.info(`Expand pool ${poolType} of cluster ${location.clusterId}: pod ${location.pod.namespace}/${location.pod.name} was scheduled to node ${location.node.name}`)
        this.expandPool(location.clusterId, poolType)
        changed = true
      }
    }
    if (changed) {
      this.saveCollapsedPools()
    }
  }

  /** True if the display object is drawn: its parent chain reaches the stage. */
  isOnStage(displayObject) {
    for (let obj = displayObject; obj; obj = obj.parent) {
      if (obj === this.stage) {
        return true
      }
    }
    return false
  }

  update() {
    // make sure we create a copy (this.clusters might get modified)
    const clusters = Array.from(this.clusters.entries()).sort().map(idCluster => idCluster[1])
    const that = this
    let changes = 0
    const podLocations = this.collectPodLocations(clusters)
    for (const key of Object.keys(ALL_PODS)) {
      const pod = ALL_PODS[key]
      if (!podLocations.has(key)) {
        // pod was deleted
        delete ALL_PODS[key]
        // animate only pods that are drawn now (not in a collapsed pool or a hidden cluster)
        if (changes < 10 && this.isOnStage(pod)) {
          // NOTE: we need to do this BEFORE removeChildren()
          // to get correct global coordinates
          const globalPos = pod.toGlobal({ x: 0, y: 0 })
          window.setTimeout(function () {
            that.animatePodDeletion(pod, globalPos)
          }, 100 * changes)
          changes++
        } else {
          pod.destroy()
        }
      }
    }
    for (const key of this.seenPods) {
      if (!podLocations.has(key)) {
        this.seenPods.delete(key)
      }
    }
    if (!this.bootstrapping) {
      // before drawing: an expanded pool gets drawn in this update
      this.expandPoolsOfScheduledPods(podLocations)
    }

    const clusterComponentById = {}
    for (const component of this.viewContainer.children) {
      clusterComponentById[component.cluster.id] = component
    }
    let y = 0
    const clusterIds = new Set()
    for (const cluster of clusters) {
      if (!this.selectedClusters.size || this.selectedClusters.has(cluster.id)) {
        clusterIds.add(cluster.id)
        const status = this.clusterStatuses.get(cluster.id)
        let clusterBox = clusterComponentById[cluster.id]
        if (!clusterBox) {
          clusterBox = new Cluster(cluster, status, this.tooltip, this.config)
          this.viewContainer.addChild(clusterBox)
        }
        clusterBox.cluster = cluster
        clusterBox.status = status
        const dataVersion = this.clusterDataVersions.get(cluster.id)
        if (clusterBox.drawnDataVersion !== dataVersion || clusterBox.drawnLayoutVersion !== this.layoutVersion) {
          try {
            clusterBox.draw()
            clusterBox.drawnDataVersion = dataVersion
            clusterBox.drawnLayoutVersion = this.layoutVersion
          } catch (e) {
            console.error('Error drawing cluster ' + cluster.id, e)
          }
        } else {
          // unchanged data and layout: no redraw, only the stale-data check
          clusterBox.updateStaleState()
        }
        clusterBox.x = 0
        clusterBox.y = y
        y += (clusterBox.drawnHeight || clusterBox.height) + 10
      }
    }
    for (const component of [...this.viewContainer.children]) {
      if (!clusterIds.has(component.cluster.id)) {
        this.viewContainer.removeChild(component)
        component.destroy()
      }
    }
    this.filter()

    for (const key of podLocations.keys()) {
      if (this.seenPods.has(key)) {
        continue
      }
      // pod was created: mark it as seen even if it is not drawn (collapsed pool, hidden cluster),
      // so it does not animate as a new pod when its pool is expanded later
      this.seenPods.add(key)
      const pod = ALL_PODS[key]
      if (pod && this.isOnStage(pod)) {
        if (!this.bootstrapping && changes < 10) {
          const globalPos = pod.toGlobal({ x: 0, y: 0 })
          window.setTimeout(function () {
            that.animatePodCreation(pod, globalPos)
          }, 100 * changes)
        }
        changes++
      }
    }

    this.podNodeNames = new Map()
    for (const [key, location] of podLocations) {
      this.podNodeNames.set(key, location.node ? location.node.name : null)
    }
    this.knownClusterIds = new Set(clusters.filter(c => c).map(c => c.id))
  }

  tick(ticker) {
    const time = ticker.deltaTime
    const deltaX = this.viewContainerTargetPosition.x - this.viewContainer.x
    const deltaY = this.viewContainerTargetPosition.y - this.viewContainer.y
    if (Math.abs(deltaX) < 20 && Math.abs(deltaY) < 20) {
      this.viewContainer.position.x = this.viewContainerTargetPosition.x
      this.viewContainer.position.y = this.viewContainerTargetPosition.y
    } else {
      if (Math.abs(deltaX) > time) {
        this.viewContainer.x += time * Math.sign(deltaX) * Math.max(10, Math.abs(deltaX) / 10)
      }
      if (Math.abs(deltaY) > time) {
        this.viewContainer.y += time * Math.sign(deltaY) * Math.max(10, Math.abs(deltaY) / 10)
      }
    }
    this.renderer.render(this.stage)
  }

  changeSorting(text, newSortFunction) {
    this.sorterFn = newSortFunction
    this.layoutVersion++
    localStorage.setItem('sorterFn', text)
    this.update()
  }

  changeStatusFilter(text, newStatusFilterFunction) {
    this.statusFilterFn = newStatusFilterFunction
    localStorage.setItem('statusFilterFn', text)
    this.update()
  }

  switchTheme(text, newTheme) {
    this.theme = Theme.get(newTheme)
    this.draw()
    this.update()
    localStorage.setItem('theme', newTheme)
  }

  toggleCluster(clusterId) {
    if (this.selectedClusters.has(clusterId)) {
      this.selectedClusters.delete(clusterId)
    } else {
      this.selectedClusters.add(clusterId)
    }
    this.changeLocationHash('clusters', Array.from(this.selectedClusters).join(','))
    // make sure we are updating our EventSource filter
    this.connect()
    this.update()
  }

  isPoolCollapsed(clusterId, poolType) {
    return this.collapsedPools.has(clusterId + ':' + poolType) || this.collapsedPools.has(poolType)
  }

  togglePoolCollapse(clusterId, poolType, allClusters = false) {
    const key = clusterId + ':' + poolType
    const isCurrentlyCollapsed = this.isPoolCollapsed(clusterId, poolType)
    if (allClusters) {
      for (const cid of this.clusters.keys()) {
        const cKey = cid + ':' + poolType
        if (isCurrentlyCollapsed) {
          this.collapsedPools.delete(cKey)
          this.collapsedPools.delete(poolType)
        } else {
          this.collapsedPools.add(cKey)
        }
      }
    } else {
      if (isCurrentlyCollapsed) {
        this.collapsedPools.delete(key)
        this.collapsedPools.delete(poolType)
      } else {
        this.collapsedPools.add(key)
      }
    }
    this.saveCollapsedPools()
    this.update()
  }

  expandPool(clusterId, poolType) {
    if (this.collapsedPools.has(poolType)) {
      // pool type collapsed in all clusters: keep it collapsed in the other clusters
      this.collapsedPools.delete(poolType)
      for (const otherClusterId of this.clusters.keys()) {
        if (otherClusterId !== clusterId) {
          this.collapsedPools.add(otherClusterId + ':' + poolType)
        }
      }
    }
    this.collapsedPools.delete(clusterId + ':' + poolType)
  }

  setClusterPoolsCollapsed(clusterId, poolTypes, collapsed) {
    for (const poolType of poolTypes) {
      if (collapsed) {
        this.collapsedPools.add(clusterId + ':' + poolType)
      } else {
        this.collapsedPools.delete(clusterId + ':' + poolType)
        this.collapsedPools.delete(poolType)
      }
    }
    this.saveCollapsedPools()
    this.update()
  }

  saveCollapsedPools() {
    // all pool collapse changes come here: redraw the clusters
    this.layoutVersion++
    localStorage.setItem('collapsedPools', JSON.stringify(Array.from(this.collapsedPools)))
    const collapsedStr = Array.from(this.collapsedPools).join(',')
    this.changeLocationHash('collapsed', collapsedStr || null)
  }

  keepAlive() {
    if (this.keepAliveTimer != null) {
      clearTimeout(this.keepAliveTimer)
    }
    this.keepAliveTimer = setTimeout(this.connect.bind(this), this.config.keepAliveSeconds * 1000)
    if (this.connectTime != null) {
      const now = Date.now()
      if (now - this.connectTime > this.config.maxConnectionLifetimeSeconds * 1000) {
        // maximum connection lifetime exceeded => reconnect
        this.connect()
      }
    }
  }

  pruneUnavailableClusters() {
    let updateNeeded = false
    const nowSeconds = Date.now() / 1000
    for (const [clusterId, statusObj] of this.clusterStatuses.entries()) {
      const lastQueryTime = statusObj.last_query_time || 0
      if (lastQueryTime < nowSeconds - this.config.maxDataAgeSeconds) {
        this.clusters.delete(clusterId)
        updateNeeded = true
      } else if (lastQueryTime < nowSeconds - 20) {
        updateNeeded = true
      }
    }
    if (updateNeeded) {
      this.update()
    }
  }

  disconnect() {
    if (this.eventSource != null) {
      this.eventSource.close()
      this.eventSource = null
      this.connectTime = null
    }
  }

  refreshLastQueryTime(clusterId) {
    let statusObj = this.clusterStatuses.get(clusterId)
    if (!statusObj) {
      statusObj = {}
    }
    statusObj.last_query_time = Date.now() / 1000
    this.clusterStatuses.set(clusterId, statusObj)
  }

  connect() {
    // first close the old connection
    this.disconnect()
    const that = this
    // NOTE: path must be relative to work with kubectl proxy out of the box
    let url = this.config.route_prefix + (this.config.route_prefix === '/' ? 'events' : '/events')
    const clusterIds = Array.from(this.selectedClusters).join(',')
    if (clusterIds) {
      url += '?cluster_ids=' + clusterIds
    }
    const eventSource = this.eventSource = new EventSource(url, { credentials: 'include' })
    this.keepAlive()
    eventSource.onerror = function (_event) {
      that._errors++
      if (that._errors <= 1) {
        // immediately reconnect on first error
        that.connect()
      } else {
        // rely on keep-alive timer to reconnect
        that.disconnect()
      }
    }
    eventSource.addEventListener('clusterupdate', function (event) {
      that._errors = 0
      that.keepAlive()
      const cluster = JSON.parse(event.data)
      const status = that.clusterStatuses.get(cluster.id)
      const nowSeconds = Date.now() / 1000
      if (status && status.last_query_time < nowSeconds - that.config.maxDataAgeSeconds) {
        // outdated data => ignore
      } else {
        that.clusters.set(cluster.id, cluster)
        that.clusterDataVersions.set(cluster.id, (that.clusterDataVersions.get(cluster.id) || 0) + 1)
        that.update()
      }
    })
    eventSource.addEventListener('clusterdelta', function (event) {
      that._errors = 0
      that.keepAlive()
      const data = JSON.parse(event.data)
      // we received some delta => we know that the cluster query succeeded!
      that.refreshLastQueryTime(data.cluster_id)
      const cluster = that.clusters.get(data.cluster_id)
      if (cluster && data.delta && data.delta.length > 0) {
        // patch in place (no deep copy of the whole cluster); the data version tells update() to redraw
        let patched
        try {
          patched = JSON_delta.patch(cluster, data.delta)
        } catch (e) {
          console.error('Failed to apply delta for cluster ' + data.cluster_id + ', reconnect to get full data', e)
          that.clusters.delete(data.cluster_id)
          that.connect()
          return
        }
        that.clusters.set(data.cluster_id, patched)
        that.clusterDataVersions.set(data.cluster_id, (that.clusterDataVersions.get(data.cluster_id) || 0) + 1)
        that.update()
      } else if (cluster) {
        // empty delta: data unchanged; update() only refreshes the stale-data state
        that.update()
      }
    })
    eventSource.addEventListener('clusterstatus', function (event) {
      that._errors = 0
      that.keepAlive()
      const data = JSON.parse(event.data)
      that.clusterStatuses.set(data.cluster_id, data.status)
    })
    eventSource.addEventListener('bootstrapend', function (_event) {
      that._errors = 0
      that.keepAlive()
      that.bootstrapping = false
    })
    this.connectTime = Date.now()
  }

  async run() {
    await this.initialize()
    this.draw()
    this.connect()

    PIXI.Ticker.shared.add(this.tick, this)
  }
}
