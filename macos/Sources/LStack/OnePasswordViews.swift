import AppKit
import SwiftUI

// MARK: - shared bits

/// `new` / `changed` / `missing` / `blocked` / `ok` / `refresh` as a small coloured capsule.
struct OPStatusBadge: View {
    let status: String

    private var info: (String, Color) {
        switch status {
        case "ok": return ("approved", .green)
        case "new": return ("new", .orange)
        case "changed": return ("changed", .orange)
        case "missing": return ("not cached", .red)
        case "blocked": return ("blocked", .red)
        case "refresh": return ("refresh", .teal)
        case "approve": return ("needs approval", .orange)
        case "notStored": return ("not stored", .red)
        default: return (status, .secondary)
        }
    }

    var body: some View {
        let (text, color) = info
        Text(text).font(.system(size: 9, weight: .semibold)).textCase(.uppercase)
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(Capsule().fill(color.opacity(0.15)))
            .foregroundStyle(color)
    }
}

/// Progress of one item in a run.
struct OPItemStateIcon: View {
    let state: OnePasswordRun.ItemState?

    var body: some View {
        switch state {
        case .reading?: ProgressView().controlSize(.mini).frame(width: 14)
        case .read?: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green).frame(width: 14)
        case .noRead?: Image(systemName: "checkmark.circle").foregroundStyle(.secondary).frame(width: 14)
        case .failed(let e)?: Image(systemName: "xmark.octagon.fill").foregroundStyle(.red).frame(width: 14).help(e)
        case .skipped?: Image(systemName: "minus.circle").foregroundStyle(.secondary).frame(width: 14).help("Not read (stopped after an earlier error)")
        case .waiting?: Image(systemName: "circle").foregroundStyle(.tertiary).frame(width: 14)
        case nil: EmptyView()
        }
    }
}

private func opLabel(_ t: String) -> some View {
    Text(t).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary).textCase(.uppercase)
}

private func opCard<C: View>(@ViewBuilder _ content: () -> C) -> some View {
    VStack(alignment: .leading, spacing: 6, content: content)
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(.background.secondary))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(.separator.opacity(0.6)))
}

/// What happened in a run: per-item errors, the import result, and a run-level error.
struct OPRunOutcome: View {
    let run: OnePasswordRun

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if run.phase == .reading {
                Label("Waiting for you to approve in the 1Password app…", systemImage: "hand.raised.fill")
                    .font(.system(size: 11)).foregroundStyle(.blue)
            }
            if run.phase == .importing {
                Label("Storing in LStack's encrypted store…", systemImage: "lock.fill").font(.system(size: 11)).foregroundStyle(.secondary)
            }
            if let plan = run.plan {
                ForEach(plan.items) { i in
                    if case .failed(let e)? = run.states[i.name] {
                        Label(e, systemImage: "xmark.octagon.fill").font(.system(size: 10)).foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                    }
                }
            }
            if run.phase == .done {
                ForEach(run.resultLines, id: \.self) { l in
                    Label(l, systemImage: "checkmark.circle.fill").font(.system(size: 11)).foregroundStyle(.green)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            if let e = run.error {
                Label(e, systemImage: "exclamationmark.triangle.fill").font(.system(size: 11)).foregroundStyle(run.phase == .done ? .orange : .red)
                    .fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
            }
        }
    }
}

/// A plan's items (what would be read and where it goes), with progress once running.
struct OPPlanList: View {
    @Bindable var run: OnePasswordRun

    var body: some View {
        if let plan = run.plan {
            VStack(alignment: .leading, spacing: 8) {
                if !plan.items.isEmpty {
                    opCard {
                        ForEach(Array(plan.items.enumerated()), id: \.element.id) { n, i in
                            if n > 0 { Divider() }
                            row(i)
                        }
                    }
                }
                if !plan.removals.isEmpty {
                    opCard {
                        opLabel("No longer in the env files")
                        ForEach(plan.removals, id: \.self) { r in
                            HStack(spacing: 6) {
                                Image(systemName: "minus.circle.fill").foregroundStyle(.orange)
                                Text(r.key).font(.system(size: 11, weight: .medium, design: .monospaced))
                                Text("(\(r.secret))").font(.system(size: 10)).foregroundStyle(.secondary)
                            }
                        }
                        Toggle("Stop passing these to \(plan.service ?? "the service")", isOn: $run.applyRemovals)
                            .toggleStyle(.checkbox).font(.system(size: 11)).disabled(run.phase != .ready)
                    }
                }
            }
        }
    }

    private func row(_ i: OPPlan.Item) -> some View {
        HStack(alignment: .top, spacing: 8) {
            OPItemStateIcon(state: run.states[i.name]).padding(.top, 1)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(i.target).font(.system(size: 12, weight: .medium, design: .monospaced))
                    OPStatusBadge(status: i.status)
                }
                Group {
                    if i.source == "op-env" {
                        Text("from the Environment")
                    } else if i.cached, let m = i.from {
                        Text("value from paired Mac **\(m)**; not read from 1Password here").foregroundStyle(.orange)
                    } else if i.cached {
                        Text("already cached; no 1Password read")
                    } else {
                        Text("vault **\(i.vault ?? "?")** · item \(i.item ?? "?") · field \(i.field ?? "?")")
                    }
                }
                .font(.system(size: 10)).foregroundStyle(.secondary)
                if i.source != "op-env" {
                    Text(i.ref).font(.system(size: 10, design: .monospaced)).foregroundStyle(.tertiary).textSelection(.enabled).lineLimit(2)
                }
                ForEach(i.warnings ?? [], id: \.self) { w in
                    Label(w, systemImage: "exclamationmark.triangle.fill").font(.system(size: 10)).foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
    }
}

private let approvalNote = "LStack reads each value with the 1Password CLI (op) — the 1Password app will ask you to approve. Values go straight into LStack's encrypted store; they're never shown, logged or saved anywhere else."

// MARK: - review window

/// One service's 1Password references (from its env files): what's approved, new or changed, and a button
/// to pull them. Nothing is read from 1Password until "Pull from 1Password" is clicked.
struct OnePasswordReviewView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    let service: String
    @State private var review: OPReview?
    @State private var loadError: String?
    @State private var run: OnePasswordRun?
    @State private var syncing = false
    @State private var syncNote: String?

    private var svc: ServiceState? { store.services.first { $0.name == service } }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            ScrollView(.vertical) {
                VStack(alignment: .leading, spacing: 10) {
                    if let review { content(review) }
                    else if let loadError { Label(loadError, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red).font(.system(size: 12)) }
                    else { ProgressView().frame(maxWidth: .infinity) }
                }
                .padding(.vertical, 2)
            }
            .frame(minHeight: 180, maxHeight: 460)
            if let run { OPRunOutcome(run: run) }
            footer
        }
        .padding(18)
        .frame(width: 560)
        .task(id: service) { await load() }
        .onChange(of: svc?.secretsPending) { if run?.busy != true { Task { await load() } } }
        .onDisappear { if let run, run.phase == .ready { Task { await run.cancel() } } }
    }

    private func load() async {
        do { review = try await store.opReview(service); loadError = nil } catch { loadError = error.localizedDescription }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "key.horizontal.fill")
                .font(.system(size: 14, weight: .semibold)).foregroundStyle(.white)
                .frame(width: 30, height: 30)
                .background(RoundedRectangle(cornerRadius: 7, style: .continuous).fill(Color.blue.gradient))
            VStack(alignment: .leading, spacing: 2) {
                Text("1Password references · \(service)").font(.system(size: 15, weight: .bold))
                Text("From the service's env files, read the way `op run --env-file` reads them.")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder private func content(_ r: OPReview) -> some View {
        if r.files.isEmpty {
            Text("\(service) has no env files. Add `envFiles:` to its definition (or pick them when adding a service).")
                .font(.system(size: 11)).foregroundStyle(.secondary)
        } else {
            opCard {
                opLabel("Env files")
                ForEach(r.files, id: \.self) { f in
                    HStack(spacing: 6) {
                        Image(systemName: f.state == "ok" ? "doc.text" : f.state == "fifo" ? "lock.doc" : "exclamationmark.triangle.fill")
                            .foregroundStyle(f.state == "ok" ? Color.secondary : f.state == "fifo" ? .blue : .red)
                        Text((f.path as NSString).abbreviatingWithTildeInPath).font(.system(size: 11, design: .monospaced)).textSelection(.enabled)
                        Spacer()
                        Text(f.state == "ok" ? "" : f.state == "fifo" ? "1Password Environment mount" : (f.error ?? f.state))
                            .font(.system(size: 10)).foregroundStyle(f.state == "fifo" ? Color.blue : .red)
                    }
                }
                if !r.plain.isEmpty {
                    Text("\(r.plain.count) plain value\(r.plain.count == 1 ? "" : "s") read from the file at each start: \(r.plain.joined(separator: ", "))")
                        .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        if !r.refs.isEmpty {
            opCard {
                opLabel("op:// references")
                ForEach(Array(r.refs.enumerated()), id: \.element.id) { n, x in
                    if n > 0 { Divider() }
                    refRow(x)
                }
            }
        } else if !r.files.isEmpty {
            Text("No op:// references in these files.").font(.system(size: 11)).foregroundStyle(.secondary)
        }
        if !r.removed.isEmpty {
            opCard {
                opLabel("No longer in the env files")
                ForEach(r.removed, id: \.self) { x in
                    HStack(spacing: 6) {
                        Image(systemName: "minus.circle.fill").foregroundStyle(.orange)
                        Text(x.key).font(.system(size: 11, weight: .medium, design: .monospaced))
                        Text("still gets \(x.secret)").font(.system(size: 10)).foregroundStyle(.secondary)
                    }
                }
                if let run {
                    Toggle("Stop passing these to \(service)", isOn: Binding(get: { run.applyRemovals }, set: { run.applyRemovals = $0 }))
                        .toggleStyle(.checkbox).font(.system(size: 11)).disabled(run.busy)
                }
            }
        }
        ForEach(r.warnings, id: \.self) { w in
            Label(w, systemImage: "exclamationmark.triangle").font(.system(size: 10)).foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func refRow(_ x: OPReview.Ref) -> some View {
        let item = run?.plan?.items.first { $0.key == x.key }
        return HStack(alignment: .top, spacing: 8) {
            OPItemStateIcon(state: item.flatMap { run?.states[$0.name] }).padding(.top, 1)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(x.key).font(.system(size: 12, weight: .medium, design: .monospaced))
                    OPStatusBadge(status: x.status)
                    if let s = x.secret, s != x.key {
                        Text("stored as \(s)").font(.system(size: 10)).foregroundStyle(.secondary)
                    }
                }
                Text("vault **\(x.vault ?? "?")** · item \(x.item ?? "?") · field \(x.field ?? "?")")
                    .font(.system(size: 10)).foregroundStyle(.secondary)
                Text(x.ref).font(.system(size: 10, design: .monospaced)).foregroundStyle(.tertiary).textSelection(.enabled).lineLimit(2)
                if let t = x.template {
                    Text("written as \(t)").font(.system(size: 10, design: .monospaced)).foregroundStyle(.tertiary)
                }
                if let item, item.cached, let m = item.from {
                    Label("Value from paired Mac \(m) — not read from 1Password here. Check it before importing.", systemImage: "desktopcomputer")
                        .font(.system(size: 10)).foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                } else if let by = x.offeredBy, !by.isEmpty {
                    Label("Shared by \(by.joined(separator: ", ")) — import needs no 1Password approval", systemImage: "desktopcomputer")
                        .font(.system(size: 10)).foregroundStyle(.blue)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let b = x.blocked {
                    Label(b, systemImage: "xmark.octagon.fill").font(.system(size: 10)).foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
                ForEach(x.warnings ?? [], id: \.self) { w in
                    Label(w, systemImage: "exclamationmark.triangle.fill").font(.system(size: 10)).foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if review?.actionable == true, run?.finished != true {
                Text(approvalNote).font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            if let syncNote {
                Text(syncNote).font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if review?.refs.contains(where: { !($0.offeredBy ?? []).isEmpty }) == true {
                    Button("Take from Paired Macs") { Task { await takeShared() } }
                        .disabled(syncing || run?.busy == true || review?.locked == true)
                        .help("Take values paired Macs share for \(service) (from their cache; no 1Password approval)")
                }
                if review?.hasCached == true {
                    Button("Refresh Cached Values") { start(["kind": "refresh", "service": service]) }
                        .disabled(run?.busy == true || review?.locked == true)
                        .help("Read the approved references for \(service) from 1Password again (asks for approval)")
                }
                Spacer()
                if run?.busy == true {
                    ProgressView().controlSize(.small)
                }
                Button(run?.finished == true || review?.actionable != true ? "Close" : "Cancel") {
                    if let run, run.phase == .ready { Task { await run.cancel() } }
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                .disabled(run?.busy == true)
                if let run, run.phase == .ready, let plan = run.plan {
                    // The plan stopped for confirmation because it uses values from a paired Mac.
                    Button(peerConfirmTitle(plan)) { Task { await run.run() } }
                        .keyboardShortcut(.defaultAction)
                        .buttonStyle(.borderedProminent)
                } else {
                    Button("Pull from 1Password") { start(["kind": "env", "service": service]) }
                        .keyboardShortcut(.defaultAction)
                        .buttonStyle(.borderedProminent)
                        .disabled(review?.actionable != true || review?.locked == true || run?.busy == true)
                }
            }
        }
    }

    private func takeShared() async {
        syncing = true
        defer { syncing = false }
        do {
            let r = try await store.opPeerSync(service: service)
            syncNote = ([r.summary] + (r.errors ?? [])).joined(separator: "\n")
        } catch {
            syncNote = error.localizedDescription
        }
        await load()
    }

    private func peerConfirmTitle(_ plan: OPPlan) -> String {
        let use = "Use \(plan.fromPeers.count) from \(plan.peerNames.joined(separator: ", "))"
        return plan.reads.isEmpty ? use : "\(use) & Read \(plan.reads.count) from 1Password"
    }

    private func start(_ body: [String: Any]) {
        let r = OnePasswordRun(store: store)
        if let old = run { r.applyRemovals = old.applyRemovals }
        r.onFinish = { await load() }
        run = r
        Task { await r.pull(body) }
    }
}

// MARK: - sheets (Settings › Secrets)

/// Link a reference, import an Environment, or refresh: shows the plan, then reads after "Read from 1Password".
struct OnePasswordSheet: View {
    enum Mode: Identifiable {
        case link
        case environment
        /// A refresh plan body and a title.
        case refresh([String: String], String)

        var id: String {
            switch self {
            case .link: return "link"
            case .environment: return "environment"
            case .refresh(let b, _): return "refresh:" + b.keys.sorted().map { "\($0)=\(b[$0] ?? "")" }.joined(separator: "&")
            }
        }
    }

    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    let mode: Mode
    /// Opened from a service's environment: link/import straight into it (no service picker).
    var fixedService: String? = nil
    @State private var run: OnePasswordRun?
    @State private var name = ""
    @State private var ref = ""
    @State private var service = ""
    @State private var key = ""
    @State private var path = ""
    @State private var readingMount = false
    @State private var formError: String?

    private var title: String {
        switch mode {
        case .link: return "Link a 1Password reference" + (fixedService.map { " to \($0)" } ?? "")
        case .environment: return "Import a 1Password Environment" + (fixedService.map { " into \($0)" } ?? "")
        case .refresh(_, let t): return t
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.system(size: 15, weight: .bold))
            if let run, run.phase != .idle, run.plan != nil {
                ScrollView(.vertical) { OPPlanList(run: run).padding(.vertical, 2) }
                    .frame(minHeight: 80, maxHeight: 360)
            } else {
                form
            }
            if let run { OPRunOutcome(run: run) }
            if let formError {
                Label(formError, systemImage: "exclamationmark.triangle.fill").font(.system(size: 11)).foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
            }
            footer
        }
        .padding(18)
        .frame(width: 520)
        .task {
            if let fixedService { service = fixedService }
            if case .refresh(let b, _) = mode {
                var body: [String: Any] = ["kind": "refresh", "names": b["name"].map { [$0] } ?? []]
                if let s = b["service"] { body["service"] = s }
                await prepare(body)
            }
        }
        .onDisappear { if let run, run.phase == .ready { Task { await run.cancel() } } }
    }

    @ViewBuilder private var form: some View {
        switch mode {
        case .link:
            VStack(alignment: .leading, spacing: 8) {
                Grid(alignment: .leading, horizontalSpacing: 8, verticalSpacing: 6) {
                    GridRow {
                        Text("Name").font(.system(size: 11))
                        TextField("API_KEY", text: $name).font(.system(size: 12, design: .monospaced))
                            .onChange(of: name) { name = SecretsSettings.sanitize(name) }
                    }
                    GridRow {
                        Text("Reference").font(.system(size: 11))
                        TextField("op://vault/item/field", text: $ref).font(.system(size: 12, design: .monospaced))
                    }
                    GridRow {
                        Text("Service").font(.system(size: 11))
                        servicePicker
                    }
                    if !service.isEmpty {
                        GridRow {
                            Text("As env var").font(.system(size: 11))
                            TextField(name.isEmpty ? "same as the name" : name, text: $key).font(.system(size: 12, design: .monospaced))
                                .onChange(of: key) { key = SecretsSettings.sanitize(key) }
                        }
                    }
                }
                .textFieldStyle(.roundedBorder).controlSize(.small)
                Text("In 1Password, use Copy Secret Reference on a field. The value is read once and cached encrypted; refresh it any time.")
                    .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        case .environment:
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 6) {
                    TextField("Path to the Environment's local .env mount", text: $path).font(.system(size: 11, design: .monospaced))
                    Button("Choose…", action: choose)
                }
                HStack(spacing: 6) {
                    Text("Pass to").font(.system(size: 11))
                    servicePicker
                }
                .controlSize(.small)
                Text("In 1Password, an Environment can be mounted as a local .env file (Destinations › Local .env file). Reading it asks for approval in the 1Password app; LStack caches the values encrypted.")
                    .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            .textFieldStyle(.roundedBorder).controlSize(.small)
        case .refresh:
            if run == nil || run?.phase == .planning { ProgressView().frame(maxWidth: .infinity) }
        }
    }

    @ViewBuilder private var servicePicker: some View {
        if let fixedService {
            Text(fixedService).font(.system(size: 12, weight: .medium))
        } else {
            Picker("", selection: $service) {
                Text("None (just store it)").tag("")
                ForEach(store.services) { s in Text(s.name).tag(s.name) }
            }
            .labelsHidden().fixedSize()
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if run?.phase == .ready, run?.plan?.reads.isEmpty == false {
                Text(approvalNote).font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if readingMount {
                    ProgressView().controlSize(.small)
                    Text("Waiting for you to approve in the 1Password app…").font(.system(size: 11)).foregroundStyle(.blue)
                } else if run?.busy == true {
                    ProgressView().controlSize(.small)
                }
                Spacer()
                if run?.finished == true {
                    Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
                } else {
                    Button("Cancel") {
                        if let run { Task { await run.cancel() } }
                        dismiss()
                    }
                    .keyboardShortcut(.cancelAction)
                    .disabled(run?.busy == true || readingMount)
                    primary
                }
            }
        }
    }

    @ViewBuilder private var primary: some View {
        if let run, run.phase == .ready, let plan = run.plan {
            Button(plan.reads.isEmpty ? "Import" : "Read \(plan.reads.count) from 1Password") { Task { await run.run() } }
                .keyboardShortcut(.defaultAction).buttonStyle(.borderedProminent)
        } else {
            switch mode {
            case .link:
                Button("Continue") {
                    var b: [String: Any] = ["kind": "link", "name": name, "ref": ref.trimmingCharacters(in: .whitespaces)]
                    if !service.isEmpty { b["service"] = service; if !key.isEmpty { b["key"] = key } }
                    Task { await prepare(b) }
                }
                .keyboardShortcut(.defaultAction).buttonStyle(.borderedProminent)
                .disabled(name.isEmpty || !ref.trimmingCharacters(in: .whitespaces).hasPrefix("op://") || run?.busy == true)
            case .environment:
                Button("Read Environment") { readEnvironment() }
                    .keyboardShortcut(.defaultAction).buttonStyle(.borderedProminent)
                    .disabled(path.isEmpty || readingMount || run?.busy == true)
            case .refresh:
                EmptyView()
            }
        }
    }

    private func prepare(_ body: [String: Any]) async {
        formError = nil
        let r = OnePasswordRun(store: store)
        run = r
        if !(await r.prepare(body)) {
            formError = r.error
            run = nil
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        panel.treatsFilePackagesAsDirectories = true
        panel.message = "Choose the 1Password Environment's local .env mount"
        if panel.runModal() == .OK, let url = panel.url { path = url.path }
    }

    /// Only from the button: opening the mount is what asks for approval in 1Password.
    private func readEnvironment() {
        let url = URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL.resolvingSymlinksInPath()
        formError = nil
        readingMount = true
        Task {
            var content: String?
            do {
                content = try await Task.detached(priority: .userInitiated) { try OnePasswordCLI.readMount(url) }.value
            } catch {
                formError = error.localizedDescription
            }
            readingMount = false
            guard let text = content else { return }
            var b: [String: Any] = ["kind": "environment", "path": url.path, "content": text]
            content = nil
            if !service.isEmpty { b["service"] = service }
            await prepare(b)
            b.removeAll()
        }
    }
}

// MARK: - Settings › Secrets › 1Password

struct OnePasswordCard: View {
    @Environment(AppStore.self) private var store
    @Binding var sheet: OnePasswordSheet.Mode?
    @State private var vaults = ""
    @State private var account = UserDefaults.standard.string(forKey: OnePasswordCLI.accountKey) ?? ""
    @State private var audit: [OPAuditEntry] = []
    @State private var showActivity = false
    @State private var error: String?

    private var op: OnePasswordSettings { store.settings?.onepassword ?? OnePasswordSettings() }
    private var linked: [SecretInfo] { (store.secrets?.secrets ?? []).filter(\.fromOnePassword) }
    private var savedVaults: String { (op.allowedVaults ?? []).joined(separator: ", ") }

    var body: some View {
        SettingsCard {
            SetupRow(icon: "key.horizontal.fill", tint: .blue, title: "1Password",
                     detail: "Pull `op://` references from services' env files, link single references, or import a 1Password Environment — from each service's Environment Variables. LStack reads values with the 1Password CLI only when you click, and the 1Password app asks you to approve. The background service never contacts 1Password.") {
                EmptyView()
            }
            Divider().padding(.leading, 52)
            HStack(spacing: 6) {
                Text("\(linked.count) cached 1Password value\(linked.count == 1 ? "" : "s")").font(.system(size: 11)).foregroundStyle(.secondary)
                Spacer()
                Button("Refresh All") { sheet = .refresh([:], "Refresh all from 1Password") }
                    .disabled(linked.isEmpty)
                    .help("Read every cached 1Password reference again (asks for approval)")
            }
            .controlSize(.small)
            .disabled(store.secrets?.locked == true)
            .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            if !store.pendingReviews.isEmpty {
                Divider().padding(.leading, 52)
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(store.pendingReviews) { s in
                        HStack(spacing: 6) {
                            Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
                            Text(s.name).font(.system(size: 12, weight: .medium))
                            Text(s.secretsPending?.summary ?? "").font(.system(size: 10)).foregroundStyle(.secondary)
                            Spacer()
                            Button("Review…") { store.openReview(s.name) }.controlSize(.small)
                        }
                    }
                }
                .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
            Divider().padding(.leading, 52)
            OPSharesList().padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            Divider().padding(.leading, 52)
            options.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            Divider().padding(.leading, 52)
            activity.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
        }
        .task {
            await store.refreshSettings()
            vaults = savedVaults
            audit = await store.opAudit(100)
        }
        .onChange(of: store.secrets?.secrets.map(\.fetchedAt)) { Task { audit = await store.opAudit(100) } }
    }

    private func toggle(_ title: String, _ key: String, _ value: Bool, help: String) -> some View {
        Toggle(title, isOn: Binding(get: { value }, set: { v in
            Task { error = await store.updateSettings(["onepassword": [key: v]]) }
        }))
        .toggleStyle(.checkbox).font(.system(size: 11))
        .help(help)
    }

    private var options: some View {
        VStack(alignment: .leading, spacing: 6) {
            toggle("Watch env files for new references", "watch", op.watch ?? true,
                   help: "Flag new or changed op:// lines in services' env files for review (nothing is read until you approve)")
            toggle("Stop passing references removed from env files without asking", "autoApplyRemovals", op.autoApplyRemovals ?? true,
                   help: "It only reduces what a service gets; the cached value is forgotten when nothing uses it")
            toggle("Restart running services when their values change", "restartOnChange", op.restartOnChange ?? false,
                   help: "After an import or refresh; otherwise they pick up new values on their next restart")
            toggle("Share refreshed values with Macs you've shared them with", "pushToPeers", op.pushToPeers ?? true,
                   help: "After a refresh here, send changed values to paired Macs that were granted them")
            toggle("Ask paired Macs for missing shared values (from their cache; never 1Password)", "pullFromPeers", op.pullFromPeers ?? false,
                   help: "When a value that came from a paired Mac is missing here, ask that Mac again")
            HStack(spacing: 6) {
                Text("Allowed vaults").font(.system(size: 11)).frame(width: 90, alignment: .leading)
                TextField("Any vault", text: $vaults).onSubmit(saveVaults)
                Button("Save", action: saveVaults).disabled(vaults == savedVaults)
            }
            HStack(spacing: 6) {
                Text("Account").font(.system(size: 11)).frame(width: 90, alignment: .leading)
                TextField("Default (only needed with several accounts)", text: $account)
                    .onChange(of: account) { UserDefaults.standard.set(account.trimmingCharacters(in: .whitespaces), forKey: OnePasswordCLI.accountKey) }
            }
            Text("Allowed vaults: comma separated; references into other vaults are blocked. Account: sign-in address or ID, passed to op as OP_ACCOUNT.")
                .font(.system(size: 10)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill").font(.system(size: 11)).foregroundStyle(.red)
            }
        }
        .textFieldStyle(.roundedBorder)
        .controlSize(.small)
    }

    private func saveVaults() {
        let list = vaults.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        Task {
            error = await store.updateSettings(["onepassword": ["allowedVaults": list]])
            vaults = savedVaults
        }
    }

    private var activity: some View {
        VStack(alignment: .leading, spacing: 3) {
            Button {
                withAnimation(.easeInOut(duration: 0.15)) { showActivity.toggle() }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "chevron.right").font(.system(size: 8, weight: .bold)).rotationEffect(.degrees(showActivity ? 90 : 0))
                    opLabel("Recent activity")
                    if !showActivity { Text("\(audit.count) event\(audit.count == 1 ? "" : "s")").font(.system(size: 10)) }
                    Spacer()
                }
                .foregroundStyle(.secondary).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if showActivity {
                if audit.isEmpty {
                    Text("Nothing yet.").font(.system(size: 10)).foregroundStyle(.tertiary)
                }
                ForEach(Array(audit.reversed().prefix(30).enumerated()), id: \.offset) { _, e in
                    HStack(spacing: 4) {
                        if let d = e.date {
                            Text(d, format: .dateTime.month(.abbreviated).day().hour().minute()).foregroundStyle(.tertiary)
                        } else {
                            Text(e.ts).foregroundStyle(.tertiary)
                        }
                        Text("\(e.actor) · \(e.action)").foregroundStyle(.secondary)
                        Text(e.summary).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
                    }
                    .font(.system(size: 10))
                }
            }
        }
    }
}

// MARK: - sharing with paired Macs

/// Cached 1Password values shared with paired Macs over the encrypted link. Never contacts 1Password.
struct OPSharesList: View {
    @Environment(AppStore.self) private var store
    @State private var shares: OPShares?
    @State private var busy = false
    @State private var message: String?
    @State private var error: String?

    private var peers: [OPShares.Peer] { shares?.peers ?? [] }
    private var received: [OPShares.Received] { shares?.received ?? [] }
    private var available: [OPShares.Available] { shares?.available ?? [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 6) {
                opLabel("Shared with Macs")
                Spacer()
                Button("Take Shared Values") { Task { await sync() } }
                    .disabled(busy || (available.isEmpty && received.isEmpty))
                    .help("Take the values paired Macs share with this Mac now (from their cache; never 1Password)")
            }
            if peers.isEmpty && received.isEmpty && available.isEmpty {
                Text("No paired Macs share 1Password values with this Mac, and none have asked for any.")
                    .font(.system(size: 10)).foregroundStyle(.tertiary)
            }
            ForEach(peers) { p in peerRows(p) }
            ForEach(received, id: \.self) { r in
                HStack(spacing: 6) {
                    Image(systemName: "arrow.down.circle").foregroundStyle(.blue)
                    Text(r.name).font(.system(size: 11, weight: .medium, design: .monospaced))
                    Text(ageText("from \(r.from)", r.fetchedAt)).font(.system(size: 10)).foregroundStyle(.secondary)
                    Spacer()
                }
                .help(r.ref ?? "")
            }
            let waiting = available.filter { a in !received.contains { $0.ref == a.ref } }
            if !waiting.isEmpty {
                Text("Offered to this Mac: " + waiting.map { "\(shortRef($0.ref)) (\($0.machine))" }.joined(separator: ", "))
                    .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            if let message {
                Text(message).font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill").font(.system(size: 11)).foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .controlSize(.small)
        .task { await load() }
        .onChange(of: store.secrets?.secrets.map(\.fetchedAt)) { Task { await load() } }
    }

    @ViewBuilder private func peerRows(_ p: OPShares.Peer) -> some View {
        let asks = (p.wants ?? []).filter { $0.have == true && $0.granted != true }
        let granted = p.granted ?? []
        if !asks.isEmpty || !granted.isEmpty {
            HStack(spacing: 5) {
                Image(systemName: "desktopcomputer").foregroundStyle(.secondary)
                Text(p.name).font(.system(size: 11, weight: .semibold))
            }
            .padding(.top, 2)
            ForEach(asks, id: \.self) { w in
                HStack(spacing: 6) {
                    Text(w.name ?? shortRef(w.ref)).font(.system(size: 11, design: .monospaced))
                    Text("asked for it").font(.system(size: 10)).foregroundStyle(.orange)
                    Spacer()
                    Button("Share") { Task { await share(p, w.name ?? w.ref, allow: true) } }.disabled(busy)
                        .help("Let \(p.name) take this cached value over the encrypted link (\(w.ref))")
                }
                .padding(.leading, 18)
            }
            ForEach(granted, id: \.self) { g in
                HStack(spacing: 6) {
                    Text(g.name ?? shortRef(g.ref)).font(.system(size: 11, design: .monospaced))
                    Text(g.have == false ? "shared · not cached here" : "shared").font(.system(size: 10)).foregroundStyle(.secondary)
                    Spacer()
                    Button("Stop Sharing") { Task { await share(p, g.name ?? g.ref, allow: false) } }.disabled(busy)
                        .help(g.ref)
                }
                .padding(.leading, 18)
            }
        }
    }

    private func shortRef(_ ref: String) -> String {
        ref.hasPrefix("op://") ? String(ref.dropFirst(5)) : ref
    }

    private func ageText(_ prefix: String, _ ms: Double?) -> String {
        guard let ms else { return prefix }
        let rel = RelativeDateTimeFormatter().localizedString(for: Date(timeIntervalSince1970: ms / 1000), relativeTo: .now)
        return "\(prefix) · fetched \(rel)"
    }

    private func load() async {
        do { shares = try await store.opShares(); error = nil } catch { self.error = error.localizedDescription }
    }

    private func share(_ p: OPShares.Peer, _ item: String, allow: Bool) async {
        busy = true
        defer { busy = false }
        do { shares = try await store.opShare(peer: p.id, items: [item], allow: allow); error = nil } catch { self.error = error.localizedDescription }
    }

    private func sync() async {
        busy = true
        defer { busy = false }
        do {
            let r = try await store.opPeerSync()
            message = r.summary
            error = (r.errors ?? []).isEmpty ? nil : r.errors!.joined(separator: "\n")
            await load()
        } catch { self.error = error.localizedDescription }
    }
}
