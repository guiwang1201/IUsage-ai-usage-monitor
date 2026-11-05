import AppKit
import CoreGraphics

final class Prototype: NSObject, NSApplicationDelegate {
    private let live = CommandLine.arguments.contains("--live") || Bundle.main.object(forInfoDictionaryKey: "IUsageDefaultLive") as? Bool == true
    private var cachedOnly: Bool { live || CommandLine.arguments.contains("--cached") }
    private var loadingCredentials = false
    private var activeTransports: [UsageHTTP] = []
    private var coordinator: FetchCoordinator!
    private var item: NSStatusItem!
    private let bars = UsageBars(frame: NSRect(x: 0, y: 0, width: 148, height: 22))
    private let popover = NSPopover()
    private var selector: NSSegmentedControl!
    private var instanceLock: InstanceLock?
    private var runtime: OfflineRuntime!
    private var timer: Timer?
    private var sectionResets: [[NSTextField]] = []
    private var sectionStatuses: [NSTextField] = []
    private var sectionPaces: [NSTextField] = []
    private var sectionPercentages: [[NSTextField]] = []
    private var sectionProgress: [[NSProgressIndicator]] = []
    private var activityTimer: Timer?
    private var wasIdle = false
    private var wakeObserver: NSObjectProtocol?
    private let mockBodies = [
        #"{"five_hour":{"utilization":32},"seven_day":{"utilization":64}}"#,
        #"{"rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000},"secondary_window":{"used_percent":82,"limit_window_seconds":604800}}}"#
    ]

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        SystemFeatures.configureNotifications()
        do {
            let directory = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
                .appendingPathComponent(cachedOnly ? "IUsage" : "IUsagePrototype", isDirectory: true)
            instanceLock = try InstanceLock(url: directory.appendingPathComponent("instance.lock"))
            let store = OfflineStore(url: directory.appendingPathComponent(cachedOnly ? "cache.json" : "mock-cache.json"))
            runtime = OfflineRuntime(state: try store.load(), save: store.save)
            let now = Int64(Date().timeIntervalSince1970)
            for provider in 0...1 where !cachedOnly && runtime.nextEvent(provider: provider, now: now, idle: false) <= now {
                try runtime.simulate(provider: provider, now: now, status: 200, body: mockBodies[provider])
            }
        } catch {
            // Duplicate instances or unavailable storage must not begin scheduling.
            NSApp.terminate(nil)
            return
        }
        coordinator = FetchCoordinator(runtime: runtime, now: { Int64(Date().timeIntervalSince1970) }) { [weak self] url, headers, callback in
            let transport = UsageHTTP(configuration: .ephemeral)
            self?.activeTransports.append(transport)
            transport.get(url: url, headers: headers) { result in
                callback(result)
                DispatchQueue.main.async { self?.activeTransports.removeAll { $0 === transport } }
            }
        }
        item = NSStatusBar.system.statusItem(withLength: 148)
        item.autosaveName = "IUsageStatus"
        if let button = item.button {
            bars.frame = button.bounds
            bars.autoresizingMask = [.width, .height]
            button.addSubview(bars)
            button.target = self
            button.action = #selector(statusClick)
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
            button.toolTip = "IUsage · 离线原型（模拟数据）"
            button.setAccessibilityLabel("IUsage 模拟数据：Claude，5 小时已用 32%，7 天已用 64%")
        }
        let controller = NSViewController()
        let root = NSView(frame: NSRect(x: 0, y: 0, width: 370, height: 470))
        controller.view = root
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: root.topAnchor, constant: 18)
        ])
        stack.addArrangedSubview(label("IUsage", size: 17, weight: .semibold))
        let notice = label(live ? "用量监视 · 定时刷新开启" : cachedOnly ? "真实缓存 · 自动刷新未开启" : "离线原型 · 以下均为模拟数据", size: 11)
        notice.textColor = .secondaryLabelColor
        stack.addArrangedSubview(notice)
        selector = NSSegmentedControl(labels: ["Claude", "Codex"], trackingMode: .selectOne, target: self, action: #selector(selectProvider))
        selector.selectedSegment = runtime.state.selectedProvider
        selector.toolTip = "选择菜单栏显示的服务"
        stack.addArrangedSubview(selector)
        stack.addArrangedSubview(section("Claude", values: [PreviewUsage.claude.five, PreviewUsage.claude.seven], resets: [cachedOnly ? "重置时间待接入" : "2 小时 18 分钟后重置", cachedOnly ? "" : "3 天后重置"]))
        stack.addArrangedSubview(section("Codex", values: [PreviewUsage.codex.five, PreviewUsage.codex.seven], resets: [cachedOnly ? "重置时间待接入" : "4 小时后重置", cachedOnly ? "" : "1 天 6 小时后重置"]))
        let footer = NSStackView()
        footer.orientation = .horizontal
        footer.spacing = 14
        let hint = label(live ? "正常 \(requestGap) 秒 · 空闲 15 分钟刷新" : cachedOnly ? "只读缓存 · 无网络请求" : "模拟调度 · 未连接服务", size: 10)
        hint.textColor = .secondaryLabelColor
        footer.addArrangedSubview(hint)
        footer.addArrangedSubview(NSButton(title: "退出", target: self, action: #selector(quit)))
        stack.addArrangedSubview(footer)
        popover.contentViewController = controller
        popover.contentSize = root.frame.size
        popover.behavior = .transient
        popover.animates = false
        updateBars()
        updateDetails()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in self?.fitStatusItem() }
        scheduleNext()
        // Coarse idle observation needs no input-monitoring permission. Only the
        // transition back to activity reschedules; it never clears cooldown.
        if !cachedOnly || live { activityTimer = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.fitStatusItem()
            let idle = CGEventSource.secondsSinceLastEventType(.combinedSessionState, eventType: .null) >= 300
            if self.wasIdle && !idle { self.scheduleNext() }
            self.wasIdle = idle
        }
        activityTimer?.tolerance = 5 }
        wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [weak self] _ in
            self?.scheduleNext()
        }
    }
    private func fitStatusItem() {
        guard let window = item.button?.window else { return }
        // Keep the full readout when it fits; shorten only the tracks when the
        // system places this wide item under the camera housing.
        let fullWidth = bars.requiredWidth(compact: false)
        let extraWidth = fullWidth - item.length
        let fits = window.screen?.auxiliaryTopRightArea.map { window.frame.minX - extraWidth >= $0.minX } ?? true
        bars.compact = !fits
        let width = bars.requiredWidth(compact: !fits)
        if item.length != width {
            item.length = width
            if let button = item.button { bars.frame = NSRect(x: 0, y: 0, width: width, height: button.bounds.height) }
            bars.needsDisplay = true
        }
    }
    private func label(_ text: String, size: CGFloat, weight: NSFont.Weight = .regular) -> NSTextField {
        let field = NSTextField(labelWithString: text)
        field.font = NSFont.systemFont(ofSize: size, weight: weight)
        return field
    }
    private func section(_ title: String, values: [Double], resets: [String]) -> NSView {
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 5
        stack.addArrangedSubview(label(title, size: 13, weight: .semibold))
        let status = label("", size: 10)
        status.textColor = .secondaryLabelColor
        sectionStatuses.append(status)
        stack.addArrangedSubview(status)
        var resetLabels: [NSTextField] = []
        var percentages: [NSTextField] = []
        var indicators: [NSProgressIndicator] = []
        for i in 0..<2 {
            let row = NSStackView()
            row.orientation = .horizontal
            row.spacing = 8
            let text = label(i == 0 ? "5 小时" : "7 天", size: 11)
            text.widthAnchor.constraint(equalToConstant: 40).isActive = true
            row.addArrangedSubview(text)
            let progress = NSProgressIndicator()
            progress.isIndeterminate = false
            progress.minValue = 0
            progress.maxValue = 100
            progress.doubleValue = values[i]
            progress.style = .bar
            progress.widthAnchor.constraint(equalToConstant: 190).isActive = true
            progress.setAccessibilityLabel("\(title) \(i == 0 ? "5 小时" : "7 天")用量")
            row.addArrangedSubview(progress)
            let percentage = label("\(Int(values[i]))%", size: 11)
            percentages.append(percentage)
            indicators.append(progress)
            row.addArrangedSubview(percentage)
            stack.addArrangedSubview(row)
            let reset = label(resets[i], size: 10)
            reset.textColor = .secondaryLabelColor
            resetLabels.append(reset)
            stack.addArrangedSubview(reset)
        }
        // Whether the quota should last until it resets, below the rows as on Windows.
        let pace = label("", size: 12)
        pace.isHidden = true
        sectionPaces.append(pace)
        stack.setCustomSpacing(10, after: stack.arrangedSubviews.last!)
        stack.addArrangedSubview(pace)
        sectionResets.append(resetLabels)
        sectionPercentages.append(percentages)
        sectionProgress.append(indicators)
        return stack
    }
    private func updateDetails() {
        let now = Int64(Date().timeIntervalSince1970)
        for provider in 0...1 {
            let reading = runtime.state.readings[provider]
            let age = reading.map { max(0, now - $0.fetchedAt) }
            let updated = age.map { "\($0 / 60) 分钟前更新" } ?? "暂无数据"
            let issue = runtime.issue(provider)
            let source = !cachedOnly ? "模拟数据" : provider == 0 ? (runtime.state.claudeSessionIdentity == nil ? "旧缓存" : "Claude 网页") : "Codex"
            sectionStatuses[provider].stringValue = issue.map { reading == nil ? "\($0) · 暂无数据" : "\($0) · 显示缓存 · \(updated)" } ?? "\(source) · \(updated)"
            let values = reading?.values(now: now) ?? [nil, nil]
            // Only judged from a fresh reading, as on Windows; the mock readings have no reset times.
            let pace = issue == nil ? reading.map { PacePolicy.line(reading: $0, now: now) } : nil
            sectionPaces[provider].stringValue = pace?.text ?? ""
            sectionPaces[provider].textColor = pace?.warn == true ? .systemOrange : .labelColor
            sectionPaces[provider].isHidden = pace?.text.isEmpty ?? true
            for index in 0...1 {
                if cachedOnly {
                    let reset = index == 0 ? reading?.fiveReset : reading?.sevenReset
                    sectionResets[provider][index].stringValue = reset.map { timestamp in
                        if timestamp <= now { return "窗口已重置 · 等待下次刷新确认" }
                        let date = Date(timeIntervalSince1970: TimeInterval(timestamp))
                        let formatter = DateFormatter()
                        formatter.dateFormat = "MM-dd HH:mm"
                        return "\(DisplayFormat.longCountdown(timestamp - now))后重置 · \(formatter.string(from: date))"
                    } ?? "服务未提供重置时间（旧缓存可能未保存）"
                }
                sectionPercentages[provider][index].stringValue = values[index].map { "\(runtime.state.showRemaining == true ? "剩余" : "已用") \(Int((runtime.state.showRemaining == true ? 100 - $0 : $0).rounded()))%" } ?? "--"
                sectionProgress[provider][index].doubleValue = values[index].map { runtime.state.showRemaining == true ? 100 - $0 : $0 } ?? 0
                sectionProgress[provider][index].alphaValue = issue == nil ? 1 : 0.55
            }
        }
    }
    @objc private func statusClick() {
        guard let event = NSApp.currentEvent, let button = item.button else { toggle(); return }
        if event.type == .rightMouseUp || event.modifierFlags.contains(.control) {
            showSettings()
        } else if button.convert(event.locationInWindow, from: nil).x < 17 {
            chooseProvider(1 - runtime.state.selectedProvider)
        } else { toggle() }
    }
    private func showSettings() {
        popover.performClose(nil)
        let menu = NSMenu()
        menu.autoenablesItems = false
        for (title, tag) in [("Claude", 0), ("Codex", 1), ("显示已用", 2), ("显示剩余", 3)] {
            let entry = NSMenuItem(title: title, action: #selector(settingSelected(_:)), keyEquivalent: "")
            entry.target = self; entry.tag = tag
            entry.state = (tag < 2 ? tag == runtime.state.selectedProvider : (tag == 3) == (runtime.state.showRemaining == true)) ? .on : .off
            menu.addItem(entry)
            if tag == 1 { menu.addItem(.separator()) }
        }
        menu.addItem(.separator())
        let login = NSMenuItem(title: "配置 Claude SessionKey…", action: #selector(configureClaude), keyEquivalent: "")
        login.target = self; login.isEnabled = live && !loadingCredentials && activeTransports.isEmpty
        menu.addItem(login)
        let notifications = NSMenuItem(title: "低额度提醒", action: #selector(settingSelected(_:)), keyEquivalent: "")
        notifications.target = self; notifications.tag = 5
        notifications.state = runtime.state.notifications == true ? .on : .off
        notifications.isEnabled = live
        menu.addItem(notifications)
        let startup = NSMenuItem(title: SystemFeatures.installed ? "登录时启动" : "登录时启动（需安装到 Applications）", action: #selector(settingSelected(_:)), keyEquivalent: "")
        startup.target = self; startup.tag = 6
        startup.state = SystemFeatures.startupEnabled ? .on : .off
        startup.isEnabled = live && SystemFeatures.installed
        menu.addItem(startup)
        menu.addItem(.separator())
        let exit = NSMenuItem(title: "退出", action: #selector(quit), keyEquivalent: "")
        exit.target = self; menu.addItem(exit)
        item.menu = menu
        item.button?.performClick(nil)
        item.menu = nil
    }
    @objc private func settingSelected(_ sender: NSMenuItem) {
        if sender.tag == 5 {
            if runtime.state.notifications == true { try? runtime.setNotifications(false) }
            else { SystemFeatures.requestNotifications { [weak self] granted in
                guard let self else { return }; try? self.runtime.setNotifications(granted)
            } }
            return
        }
        if sender.tag == 6 {
            do { try SystemFeatures.setStartup(!SystemFeatures.startupEnabled) }
            catch { let alert = NSAlert(); alert.messageText = "无法修改登录启动"; alert.informativeText = "请检查系统设置中的登录项，或先安装到 Applications。"; alert.runModal() }
            return
        }
        if sender.tag < 2 { chooseProvider(sender.tag); return }
        do { try runtime.setRemaining(sender.tag == 3) }
        catch { return }
        updateBars(); updateDetails()
    }
    private func chooseProvider(_ provider: Int) {
        do { try runtime.select(provider) }
        catch { return }
        selector.selectedSegment = provider
        updateBars(); updateDetails(); scheduleNext()
    }
    @objc private func toggle() {
        if popover.isShown { popover.performClose(nil) }
        else if let button = item.button {
            NSApp.activate(ignoringOtherApps: true)
            updateDetails()
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        }
        scheduleNext()
    }
    private func updateBars() {
        let provider = runtime.state.selectedProvider
        let reading = runtime.state.readings[provider]
        let fallback = provider == 0 ? PreviewUsage.claude : PreviewUsage.codex
        bars.provider = provider == 0 ? "C" : "X"
        bars.values = reading.map { $0.values(now: Int64(Date().timeIntervalSince1970)).map { $0.map { $0 / 100 } } }
            ?? (cachedOnly ? [nil, nil] : [fallback.five / 100, fallback.seven / 100])
        bars.remaining = runtime.state.showRemaining == true
        bars.resets = [reading?.fiveReset, reading?.sevenReset]
        let issue = runtime.issue(provider)
        bars.stale = issue != nil
        fitStatusItem()
        let mode = bars.remaining ? "剩余" : "已用"
        let shown = bars.values.map { $0.map { bars.remaining ? 1 - $0 : $0 } }
        let percentageText = shown.map { $0.map { "\(Int(($0 * 100).rounded()))%" } ?? "--" }
        let summary = "IUsage \(live ? "定时刷新" : cachedOnly ? "真实缓存" : "模拟数据")：\(provider == 0 ? "Claude" : "Codex")，5h \(mode) \(percentageText[0])，7d \(mode) \(percentageText[1])\(issue.map { "（\($0)）" } ?? "")"
        item.button?.setAccessibilityLabel(summary)
        item.button?.toolTip = summary
        if popover.isShown { updateDetails() }
    }
    private var userAway: Bool {
        !popover.isShown && CGEventSource.secondsSinceLastEventType(.combinedSessionState, eventType: .null) >= 300
    }
    // After a local credential problem nothing was reserved, so the gate says "due
    // now". Waiting on a separate timer keeps this from becoming a tight loop. It
    // reads local storage only; the gate still decides whether a request follows.
    private func scheduleRecheck(after seconds: Int) {
        timer?.invalidate()
        let next = Timer(timeInterval: TimeInterval(userAway ? max(seconds, 300) : seconds), target: self, selector: #selector(recheckCredentials), userInfo: nil, repeats: false)
        next.tolerance = 2
        RunLoop.main.add(next, forMode: .common)
        timer = next
    }
    @objc private func recheckCredentials() { fetchLive(provider: runtime.state.selectedProvider, honoringSchedule: true) }
    private func scheduleNext() {
        timer?.invalidate()
        guard !cachedOnly || live else { return }
        if runtime.state.selectedProvider == 0 && runtime.state.claudeSessionExpired == true { return }
        let now = Int64(Date().timeIntervalSince1970)
        let due = runtime.nextEvent(provider: runtime.state.selectedProvider, now: now, idle: userAway)
        let next = Timer(timeInterval: TimeInterval(max(1, due - now)), target: self, selector: #selector(mockTick), userInfo: nil, repeats: false)
        next.tolerance = 1
        RunLoop.main.add(next, forMode: .common)
        timer = next
    }
    @objc private func mockTick() {
        let provider = runtime.state.selectedProvider
        let now = Int64(Date().timeIntervalSince1970)
        if runtime.nextEvent(provider: provider, now: now, idle: userAway) > now { scheduleNext(); return }
        if live { fetchLive(provider: provider); return }
        do {
            try runtime.simulate(provider: provider, now: Int64(Date().timeIntervalSince1970), status: 200, body: mockBodies[provider])
            updateBars()
            scheduleNext()
        } catch {
            // Stop on persistence failure rather than retrying a tight loop.
            timer?.invalidate()
        }
    }
    private func fetchLive(provider: Int, honoringSchedule: Bool = false) {
        guard !loadingCredentials else { return }
        loadingCredentials = true
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let sources = LocalSources(home: FileManager.default.homeDirectoryForCurrentUser, environment: ProcessInfo.processInfo.environment, reader: CredentialReader())
            let session = provider == 0 ? Result { try ClaudeSession.load() } : nil
            let loaded = provider == 1 ? Result { try sources.codex() } : nil
            DispatchQueue.main.async {
                guard let self else { return }
                self.loadingCredentials = false
                do {
                    if let session { _ = try session.get() }
                    if let loaded { _ = try loaded.get() }
                } catch {
                    if provider == 0 {
                        self.runtime.setLocalIssue(0, "请配置 Claude SessionKey（右键菜单）· 登录信息不可用")
                        self.refreshDisplay(); self.scheduleRecheck(after: 60)
                    } else { self.hold(provider, LocalIssue(error, provider: provider)) }
                    return
                }
                if self.runtime.localIssues[provider] != nil {
                    self.runtime.setLocalIssue(provider, nil)
                    self.refreshDisplay()
                }
                // A recheck only refreshes the credential status; the schedule still
                // decides when the next request may go out.
                if honoringSchedule, self.runtime.nextEvent(provider: provider, now: Int64(Date().timeIntervalSince1970), idle: self.userAway) > Int64(Date().timeIntervalSince1970) {
                    self.scheduleNext()
                    return
                }
                do {
                    let completion: (Error?) -> Void = { [weak self] error in
                        guard let self else { return }
                        self.updateBars()
                        guard error == nil else { self.pause(provider); return }
                        do {
                            let lines = try self.runtime.consumeAlert(provider: provider, now: Int64(Date().timeIntervalSince1970))
                            if !lines.isEmpty { SystemFeatures.notify(provider: provider == 0 ? "Claude" : "Codex", lines: lines) }
                        } catch { self.pause(provider); return }
                        self.scheduleNext()
                    }
                    let started: Bool
                    if let session {
                        started = try self.coordinator.fetchClaude(session: session.get(), completion: completion)
                    } else {
                        let credential = try loaded!.get()
                        started = try self.coordinator.fetch(provider: 1, token: credential.accessToken, account: credential.accountID, completion: completion)
                    }
                    if !started { self.scheduleNext() }
                } catch UsageHTTP.Failure.invalidEndpoint {
                    self.hold(provider, .invalidRequestInput)
                } catch {
                    self.pause(provider)
                }
            }
        }
    }
    @objc private func configureClaude() {
        guard !loadingCredentials, activeTransports.isEmpty else { return }
        timer?.invalidate()
        loadingCredentials = true
        defer { loadingCredentials = false; scheduleNext() }
        let alert = NSAlert()
        alert.messageText = "Claude 网页登录"
        alert.informativeText = "在 claude.ai 登录后，从浏览器开发者工具的 Cookie 中复制 sessionKey。只保存在本机钥匙串，不依赖 Claude Code。组织 ID 可留空；多个组织时需手动指定。保存不会清除限流等待。"
        let fields = NSStackView(); fields.orientation = .vertical; fields.spacing = 8
        let key = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 340, height: 24))
        key.placeholderString = "sessionKey（请粘贴新值）"
        let org = NSTextField(frame: NSRect(x: 0, y: 0, width: 340, height: 24))
        org.placeholderString = "组织 ID（可选）"
        fields.addArrangedSubview(key); fields.addArrangedSubview(org)
        fields.frame = NSRect(x: 0, y: 0, width: 340, height: 60)
        alert.accessoryView = fields
        alert.addButton(withTitle: "保存"); alert.addButton(withTitle: "取消"); alert.addButton(withTitle: "清除登录")
        NSApp.activate(ignoringOtherApps: true)
        let choice = alert.runModal()
        guard choice != .alertSecondButtonReturn else { return }
        do {
            let session = choice == .alertThirdButtonReturn ? nil : try ClaudeSession(key.stringValue, organization: org.stringValue)
            try ClaudeSession.save(session)
            try runtime.configureClaude(identity: session?.identity)
            refreshDisplay(); scheduleNext()
        } catch {
            let failure = NSAlert(); failure.messageText = "无法保存 Claude 登录"
            failure.informativeText = "请检查 SessionKey、组织 ID 格式及钥匙串访问权限。"; failure.runModal()
        }
    }
    private func refreshDisplay() { updateBars(); updateDetails() }
    private func hold(_ provider: Int, _ issue: LocalIssue) {
        runtime.setLocalIssue(provider, issue.label)
        refreshDisplay()
        scheduleRecheck(after: issue.recheckSeconds)
    }
    // The cache could not be saved. Stop instead of looping; a restart or a provider
    // switch tries again.
    private func pause(_ provider: Int) {
        timer?.invalidate()
        runtime.setLocalIssue(provider, "无法保存缓存 · 自动刷新已暂停")
        refreshDisplay()
    }
    @objc private func selectProvider() {
        do { try runtime.select(selector.selectedSegment) }
        catch { selector.selectedSegment = runtime.state.selectedProvider; return }
        updateBars()
        scheduleNext()
    }
    @objc private func quit() { NSApp.terminate(nil) }
}
let app = NSApplication.shared
let delegate = Prototype()
app.delegate = delegate
app.run()
