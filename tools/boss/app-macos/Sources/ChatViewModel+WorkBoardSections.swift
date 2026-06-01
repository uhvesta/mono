import Foundation

extension ChatViewModel {
    /// Bucket completed tasks by recency for the Done lane:
    ///   Today | Yesterday | <weekday names back to start of current week>
    ///   | Last Week | Earlier
    /// Bucketing uses `updated_at` because the engine doesn't yet record a
    /// dedicated `done_at` (status-transition) timestamp — see PR description.
    static func doneSections(
        items: [WorkTask],
        now: Date = Date(),
        calendar: Calendar = .current
    ) -> [WorkBoardSection] {
        let isoFractional = ISO8601DateFormatter()
        isoFractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let isoPlain = ISO8601DateFormatter()
        isoPlain.formatOptions = [.withInternetDateTime]

        let nowDay = calendar.startOfDay(for: now)
        guard let yesterdayDay = calendar.date(byAdding: .day, value: -1, to: nowDay) else {
            return [WorkBoardSection(id: "done-all", title: "Done", items: items)]
        }
        let weekday = calendar.component(.weekday, from: nowDay)
        let firstWeekday = calendar.firstWeekday
        let daysSinceStartOfWeek = (weekday - firstWeekday + 7) % 7
        guard let startOfWeek = calendar.date(byAdding: .day, value: -daysSinceStartOfWeek, to: nowDay),
              let startOfLastWeek = calendar.date(byAdding: .day, value: -7, to: startOfWeek)
        else {
            return [WorkBoardSection(id: "done-all", title: "Done", items: items)]
        }

        let weekdayFormatter = DateFormatter()
        weekdayFormatter.locale = .current
        weekdayFormatter.dateFormat = "EEEE"

        struct BucketSpec {
            let id: String
            let title: String
            let defaultExpanded: Bool
        }

        var bucketOrder: [BucketSpec] = [
            BucketSpec(id: "today", title: "Today", defaultExpanded: true),
            BucketSpec(id: "yesterday", title: "Yesterday", defaultExpanded: false),
        ]
        if daysSinceStartOfWeek >= 2 {
            for daysAgo in 2...daysSinceStartOfWeek {
                if let date = calendar.date(byAdding: .day, value: -daysAgo, to: nowDay) {
                    bucketOrder.append(
                        BucketSpec(
                            id: "weekday-\(daysAgo)",
                            title: weekdayFormatter.string(from: date),
                            defaultExpanded: false
                        )
                    )
                }
            }
        }
        bucketOrder.append(BucketSpec(id: "last-week", title: "Last Week", defaultExpanded: false))
        bucketOrder.append(BucketSpec(id: "earlier", title: "Earlier", defaultExpanded: false))

        var buckets: [String: [WorkTask]] = [:]
        for task in items {
            let bucketID = bucketID(
                for: task,
                nowDay: nowDay,
                yesterdayDay: yesterdayDay,
                startOfWeek: startOfWeek,
                startOfLastWeek: startOfLastWeek,
                calendar: calendar,
                isoFormatters: [isoFractional, isoPlain]
            )
            buckets[bucketID, default: []].append(task)
        }

        return bucketOrder.compactMap { spec -> WorkBoardSection? in
            guard let tasks = buckets[spec.id], !tasks.isEmpty else { return nil }
            let sorted = tasks.sorted { $0.updatedAt > $1.updatedAt }
            return WorkBoardSection(
                id: "done-\(spec.id)",
                title: spec.title,
                items: sorted,
                isCollapsible: true,
                defaultExpanded: spec.defaultExpanded
            )
        }
    }

    private static func bucketID(
        for task: WorkTask,
        nowDay: Date,
        yesterdayDay: Date,
        startOfWeek: Date,
        startOfLastWeek: Date,
        calendar: Calendar,
        isoFormatters: [ISO8601DateFormatter]
    ) -> String {
        guard let parsed = parseUpdatedAt(task.updatedAt, isoFormatters: isoFormatters) else {
            return "earlier"
        }
        let day = calendar.startOfDay(for: parsed)
        if day >= nowDay {
            return "today"
        }
        if day >= yesterdayDay {
            return "yesterday"
        }
        if day >= startOfWeek {
            let delta = calendar.dateComponents([.day], from: day, to: nowDay).day ?? 0
            return "weekday-\(delta)"
        }
        if day >= startOfLastWeek {
            return "last-week"
        }
        return "earlier"
    }

    /// `boss chore list --json` currently emits `updated_at` in two
    /// shapes — Unix epoch seconds as a digit string for older rows
    /// and ISO 8601 for newer ones. The UI must handle both until the
    /// data-shape canonicalization chore lands; treat all-digit
    /// strings as Unix seconds, otherwise fall back to ISO parsing.
    static func parseUpdatedAt(
        _ string: String,
        isoFormatters: [ISO8601DateFormatter]
    ) -> Date? {
        let trimmed = string.trimmingCharacters(in: .whitespaces)
        if !trimmed.isEmpty,
           trimmed.allSatisfy({ $0.isASCII && $0.isNumber }),
           let seconds = TimeInterval(trimmed) {
            return Date(timeIntervalSince1970: seconds)
        }
        for formatter in isoFormatters {
            if let date = formatter.date(from: trimmed) {
                return date
            }
        }
        return nil
    }

    func isTaskVisible(_ task: WorkTask) -> Bool {
        workItems(in: effectiveBoardColumn(for: task)).contains(where: { $0.id == task.id })
    }

    func mergeTaskRuntimes(
        _ runtimes: [WorkTaskRuntime],
        for productID: String,
        tasks: [WorkTask],
        chores: [WorkTask]
    ) {
        let productItemIDs = Set(tasks.map(\.id) + chores.map(\.id))
        taskRuntimesByID = taskRuntimesByID.filter { !productItemIDs.contains($0.key) }
        for runtime in runtimes {
            taskRuntimesByID[runtime.workItemID] = runtime
        }
    }

    func taskRuntime(for taskID: String) -> WorkTaskRuntime? {
        taskRuntimesByID[taskID]
    }
}
