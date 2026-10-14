# WidgetKit の reload budget と macOS デスクトップウィジェット

調査日: 2026-10-15。一次資料は Apple Developer Documentation、WWDC のトランスクリプト、Apple の社員が書いた Developer Forums の投稿に限った。forum の投稿には投稿者のラベル（Frameworks Engineer / DTS Engineer など）を併記する。Apple 社員でない投稿は参考扱いにして、その旨を書く。

前提: ccswap のメニューバープロセスは `~/.claude-swap-backup/widget/feed.json` を 30〜300 秒ごとに書き、ウィジェットがそれを読む。メニューバーアプリは unsandboxed の accessory app（`LSUIElement`）で、main executable は Rust のバイナリ。

## 結論

| # | 問い | 答え | 確度 |
|---|------|------|------|
| 1 | reload budget はあるか。macOS にも適用されるか | budget の仕組みはプラットフォームを区別せずに文書化されている（1 日 40〜70 回、15〜60 分おき程度）。macOS のデスクトップウィジェットに同じ数字が当てはまるとは、どこにも明記されていない。2022 年に Apple の Frameworks Engineer が「Mac のウィジェットには budget が無い」と答えているが、デスクトップウィジェットが出た Sonoma より前の発言。現行の macOS での扱いは文書化されていない | budget の存在と数字: 高（文書化）。macOS への適用: 不明 |
| 2 | `.after(60 秒後)` は守られるか | 守られない前提で設計すべき。`TimelineReloadPolicy` は「最も早い日時」を示すだけで、WidgetKit は reload の間隔に最低時間を設け、entry を約 5 分以上空けるよう求めている。`.after` による自動 reload は budget から引かれる | 高（文書化） |
| 3 | 含有アプリからの `reloadAllTimelines()` は budget を消費するか。メニューバーの accessory app は「foreground」か | 文書の例外は「containing app が foreground にある」とき。WWDC25 は「アプリが実行中に呼ぶので budget されない」と言っている。一方 DTS Engineer は watchOS について「バックグラウンドで実行中なら budget に数える」と答えている。macOS の accessory app が foreground 扱いになるかを書いた一次資料は見つからなかった | 未文書。実測が必要 |
| 4 | `Text(date, style: .timer)` / `.relative` は budget を使わずに更新されるか | 更新される。WidgetKit が extension を起動せずに表示を更新し続ける、と文書化されている。「budget を消費しない」という言い方そのものは無いが、reload を伴わない仕組みとして説明されている | 高（文書化。budget 非消費は文意から明らか） |
| 5 | `Button(intent:)` の後の reload は budget を消費するか | 消費しない。budget の例外として明記されており、reload は必ず行われる | 高（文書化） |
| 6 | Swift 以外のプロセス（Rust から Swift の static lib 経由）で WidgetCenter を呼べるか | WidgetCenter はアプリのプロセスか extension から呼ぶと書かれており、対象は「containing app に属するウィジェット」。呼び出し元をどう識別するかは文書化されていない。Rust バイナリが `.app` の `CFBundleExecutable` として起動されていれば、そのプロセスは containing app として振る舞うはず、というのは推測 | 未文書（推測） |

ccswap への示唆:

- 30〜300 秒ごとの書き込みに合わせてウィジェットを追従させることは、timeline の仕組みだけでは期待できない。budget が効いていれば、パーセンテージは 15〜60 分程度遅れうる。
- メニューバーから `reloadAllTimelines()` を呼ぶのは有効な手段で、Apple もそのための API と位置づけている。ただし budget の対象外になるかどうかは macOS の accessory app について未文書で、実機で `chronod` などのログを見て確かめる必要がある。
- 値が変わったときだけ reload を呼ぶ、リセットまでの残り時間は `Text(date, style: .relative/.timer)` で描いて reload に頼らない、という設計にしておけば、どちらに転んでも被害は小さい。

## 詳細

### 1. reload budget の有無と数字

出典: [Keeping a widget up to date](https://developer.apple.com/documentation/widgetkit/keeping-a-widget-up-to-date)（Apple Developer Documentation）。このページはプラットフォーム別に書き分けておらず、macOS 固有の記述も無い。

> To manage system load, WidgetKit uses a budget to distribute widget reloads over the course of the day. The budget allocation is dynamic and takes many factors into account, including:
> - The frequency and times the widget is visible to the user.
> - The widget’s last reload time.
> - Whether the widget’s containing app is active.

> WidgetKit maintains different budgets for each active widget the user adds to their device.

> A widget’s budget applies to a 24-hour period. WidgetKit tunes the 24-hour window to the user’s daily usage pattern, which means the daily budget doesn’t necessarily reset at exactly midnight. For a widget the user frequently views, a daily budget typically includes from 40 to 70 refreshes. This rate roughly translates to widget reloads every 15 to 60 minutes, but it’s common for these intervals to vary due to the many factors involved.

> The system takes a few days to learn the user’s behavior. During this learning period, your widget may receive more reloads than normal.

依頼文にある「40–70 reloads per day」「every 15–60 minutes」は、上の段落（見出し "Plan reloads within a budget"）が出どころ。

WWDC21 [Principles of great widgets](https://developer.apple.com/videos/play/wwdc2021/10048/):

> We call this an update budget.

> Budgets are allocated and accumulated through the day, and they’re heavily influenced by user viewing habits.

> Otherwise, widget updates may be withheld until budgets become available for a particular widget.

> All that said, though, reloads are not an every second operation.

WWDC25 [What’s new in widgets](https://developer.apple.com/videos/play/wwdc2025/278/):

> The scheduled timeline reload is budgeted by the system to help maintain performance and battery life.

> And during development and test, you can use the WidgetKit developer mode in Settings to ignore push and reload budgets for your app.

macOS に関する forum の投稿:

- Forum post by Apple staff（Frameworks Engineer、2022 年 9 月）、[Does widgets on Mac have timeline reload budget or limit?](https://developer.apple.com/forums/thread/711091)。「keeping-a-widget-up-to-date の制限は Mac にも当てはまるか」という質問への回答:
  > On Mac, Widgets do not. On iOS the limit is 72 manual refreshes per day.

  2022 年 9 月は macOS 12/13 の時期で、ウィジェットは通知センターにしか置けなかった。デスクトップウィジェット（macOS 14 Sonoma）より前の発言なので、現行の macOS にそのまま当てはまるかは分からない。「72」はドキュメントの「40 to 70」と食い違っており、ドキュメントにある数字ではない。

- Forum post by Apple staff（Rico、Frameworks Engineer / "WWDR | DTS | Software Engineer"、2024 年 12 月）、[Refreshing widgets - policy and background tasks?](https://developer.apple.com/forums/thread/770470)。質問者は iPhone と macOS Sequoia のデスクトップに同じウィジェットを置いている。ただし回答は springboard（iOS）を前提に書かれている:
  > You also will only get 72 refreshes per day. After that the system will very likely ignore your reloads until 24 hours has passed. It is also not recommended to schedule more than 24 hours worth of entries at a time. Generally around midnight there is also a free (not counting against your budget) refresh.

- 参考（Apple 社員ではない）: [thread 654331](https://developer.apple.com/forums/thread/654331) で、コミュニティの投稿者（oalejel、2020 年 9 月）が macOS の Console に出たログとして `Decremented widget refresh budget for <private> by 0.250000, budget is now <_DASWidgetBudget ...: 56.50/ 75.00>` を貼っている。macOS でも DAS（`dasd`）の widget budget が動いていた形跡だが、一次資料ではない。

まとめ: budget の仕組みはプラットフォーム共通の文書として書かれていて、macOS を除外する文言は無い。ただし macOS のデスクトップウィジェットに 40〜70 回が当てはまるかは、Apple がどこにも明記していない。Apple 社員の発言は「Mac には無い」（2022、Sonoma 以前）と「1 日 72 回」（2024、iOS 前提）に分かれる。

### 2. `TimelineReloadPolicy.after(60 秒後)`

[TimelineReloadPolicy](https://developer.apple.com/documentation/widgetkit/timelinereloadpolicy) の abstract:

> A type that indicates the earliest date WidgetKit requests a new timeline from the widget’s provider.

[after(_:)](https://developer.apple.com/documentation/widgetkit/timelinereloadpolicy/after(_:)):

> A policy that specifies a future date for WidgetKit to request a new timeline.

[Keeping a widget up to date](https://developer.apple.com/documentation/widgetkit/keeping-a-widget-up-to-date):

> WidgetKit imposes a minimum amount of time before it reloads a widget. Your timeline provider should create timeline entries that are at least about 5 minutes apart. WidgetKit may coalesce reloads across multiple widgets, affecting the exact time a widget is reloaded.

> WidgetKit tries to respect the date you specify, which may cause a significant increase in server load when multiple devices reload your widget at around the same time.

WWDC20 [Widgets Code-along, part 2](https://developer.apple.com/videos/play/wwdc2020/10035/):

> This means "atEnd" and "after" represent the earliest times a widget can be refreshed.

WWDC21 [Principles of great widgets](https://developer.apple.com/videos/play/wwdc2021/10048/):

> These automatic updates are budgeted and debited from your widget’s current available budget.

> The afterDate reload policy makes your widget eligible for reloading after the date that’s specified.

> Be cautious of near-immediate reloads.

> Asking for too many refreshes at this level of resolution can starve yourself of reloads later.

> This extra forecast data is super useful as widget reloads aren’t guaranteed to reload at the exact time that you specify.

Forum post by Apple staff（Documentation Engineer、2020 年 7 月、iOS 14 beta についての質問）、[thread 653265](https://developer.apple.com/forums/thread/653265):

> Updating every minute is far too aggressive. Widgets have a limited number of updates and if you were to try to update every minute, you'll quickly run out of updates. While debugging with Xcode these limits are not imposed, but if you're running your app outside of Xcode you'd see the behavior you're describing where your widget would stop updating.

結論: `.after(60 秒後)` の日付は「この時刻より前には reload しない」という下限で、その時刻に reload される保証は無い。ドキュメントは約 5 分の最低間隔を示し、`.after` による自動 reload は budget から引かれる（WWDC21）。60 秒刻みの更新は期待できない。

### 3. 含有アプリからの `reloadAllTimelines()` / `reloadTimelines(ofKind:)`

[Keeping a widget up to date](https://developer.apple.com/documentation/widgetkit/keeping-a-widget-up-to-date) にある budget の例外:

> Cases in which WidgetKit doesn’t count reloads against your widget’s budget include when:
> - The widget’s containing app is in the foreground.
> - The widget’s containing app has an active audio or navigation session.
> - The widget’s containing app is the current Now Playing app, as reported by MPNowPlayingInfoCenter.
> - The widget performs an app intent, such as when the user taps a button or toggles a switch.
> - The widget performs an animation.
> - The system locale changes.
> - Dynamic Type or Accessibility settings change.

WWDC20 [Meet WidgetKit](https://developer.apple.com/videos/play/wwdc2020/10028/)。トランスクリプトの "app processor widget" は "app process or widget" の誤記と思われるので、原文のまま引く:

> You can use the WidgetCenter APIs from within your app process or extension to reload your timeline.

> Reloads when your app processor widget is run in the background are budgeted by the system.

> And if your user makes a relevant change in your foreground app, you may also reload your timeline.

WWDC21 [Principles of great widgets](https://developer.apple.com/videos/play/wwdc2021/10048/):

> Now, normally, requests using this API would consume available budget, meaning that updates won’t occur until budget is available.

> However, there are a few situational exceptions that will make these reloads occur both immediately and budget-free.

> These are when your container app is foreground to the user or when your app is participating in a user session, like Navigation or Now Playing audio.

WWDC25 [What’s new in widgets](https://developer.apple.com/videos/play/wwdc2025/278/):

> Inside an app, if a data change occurs that should be reflected in its widget, WidgetCenter’s reloadAllTimelines or reloadTimelines(ofKind:) method can be called.

> Since the app is running when this API is called, the system does not budget this request.

Forum post by Apple staff（Ziqiao Chen、DTS Engineer、watchOS についての質問）、[thread 788713](https://developer.apple.com/forums/thread/788713):

> If `reloadAllTimelines` is called when your watchOS app is running in the foreground, the reload won't be counted against the budget; otherwise, it will.

同じスレッドの accepted answer。HealthKit の observer がアプリをバックグラウンドで起こした場合について:

> In this case, your app is running in the background, and so the reload is counted against the budget, according to the documentation.

Forum post by Apple staff（Ziqiao Chen、DTS Engineer、accepted answer）、[thread 819899](https://developer.apple.com/forums/thread/819899)。watchOS で workout を実行中の reload について:

> Yes, in the case where a watchOS app calls the WidgetKit API to update its widget by reloading the timeline while running an active workout in the background, the update is budget free for the widget.

#### メニューバーの accessory app は foreground か

- 文書化されていない。上の資料はどれも「foreground」が macOS の何に当たるか（frontmost app、active app、`NSApplication.isActive`、ウィンドウを出している、プロセスが起動している、など）を定義していない。`LSUIElement` の accessory app について触れた一次資料も見つからなかった。
- 資料どうしの食い違い:
  - ドキュメントと WWDC21: 例外は foreground のときと、user session（audio、navigation、Now Playing）に参加しているときだけ。
  - WWDC25: 「アプリが running なので budget されない」と、running であることを理由にしている。
  - DTS（watchOS）: アプリがバックグラウンドで running なら budget に数える。
  - budget の要素に「Whether the widget’s containing app is active」とあるので、active かどうかで配分が変わることだけは確か。
- 推測（未検証）: macOS の accessory app は、メニューを開いている間などを除けば frontmost / active にならない。iOS の基準を当てはめると、メニューバーからのバックグラウンドの reload は budget の対象になりうる。一方、Sonoma 以前の Mac には budget が無いという Apple 社員の発言（2022 年）や、WWDC25 の「running なら budget されない」という言い方を取れば、対象外の可能性もある。どちらとも決められない。

### 4. `Text(date, style: .timer)` / `.relative`

[Displaying dynamic dates in widgets](https://developer.apple.com/documentation/widgetkit/displaying-dynamic-dates):

> Show up-to-date, time-based information in your widget even when it isn’t running.

> Because your widget extension isn’t always running, you can’t directly update your widget’s content. Instead, WidgetKit renders your widget’s view on your behalf and displays the result. However, some SwiftUI views let you display content that continues updating while your widget is visible.

> Using a Text view in your widget, you can display dates and times that stay up to date onscreen.

> To display a relative time that updates automatically, use code similar to the example below:

> To display a timer that continues updating automatically, use code similar to the example below:

> To show progress toward a future date without reloading your widget, use init(timerInterval:countsDown:), as follows:

> To pair the progress view with a numeric countdown, or to show a countdown on its own without reloading your widget, use init(timerInterval:pauseTime:countsDown:showsHours:), as shown here:

[Keeping a widget up to date](https://developer.apple.com/documentation/widgetkit/keeping-a-widget-up-to-date):

> Even though your widget doesn’t run continually, it can display time-based information that WidgetKit updates live. For example, it might display a countdown timer that continues to count down even if your widget extension isn’t running.

> Alternatively, to show playback progress or a countdown without reloading at all, refer to Displaying-Dynamic-Dates.

[Text.DateStyle](https://developer.apple.com/documentation/swiftui/text/datestyle) の各 case の abstract: `relative` は "A style displaying a date as relative to now."、`timer` は "A style displaying a date as timer counting from now."

結論: `.timer` / `.relative` / `.offset`、`Text(timerInterval:)`、`ProgressView(timerInterval:)` は extension を起こさずに表示が進む。ドキュメントの「without reloading」から、budget を消費しないと読める。「budget を消費しない」という文言そのものは無い。ccswap では「リセットまでの残り時間」には使えるが、パーセンテージのようにファイルの中身で決まる値には使えない。

### 5. `Button(intent:)` の後の reload

[Keeping a widget up to date](https://developer.apple.com/documentation/widgetkit/keeping-a-widget-up-to-date) の例外一覧:

> The widget performs an app intent, such as when the user taps a button or toggles a switch.

[Adding interactivity to widgets and Live Activities](https://developer.apple.com/documentation/widgetkit/adding-interactivity-to-widgets-and-live-activities):

> When you return from the `perform()` function, the system reloads the widget’s timeline using its timeline provider.

> Interactions with a toggle or button always guarantee a timeline reload.

> By default, the system runs the app intent in the same process as the widget extension.

WWDC23 [Bring widgets to life](https://developer.apple.com/videos/play/wwdc2023/10028/):

> Typically, updates to widgets are done on a best effort basis, but importantly, reloads initiated from an interaction are always guaranteed to occur.

> As soon as your perform returns, the system will immediately initiate a reload of your widget timeline, giving you the opportunity to update the content of your widget.

結論: 文書化されている。intent を実行した直後の reload は budget の対象外で、必ず行われる。「更新」ボタンを置けば、押した時点で最新の `widget.json` を読み直せる。macOS 固有の例外は書かれていない。

### 6. Swift 以外のプロセスから WidgetCenter を呼べるか

一次資料にある記述:

- [WidgetCenter](https://developer.apple.com/documentation/widgetkit/widgetcenter) は macOS 11.0 から使える（ページの availability メタデータ）。`reloadAllTimelines()` / `reloadTimelines(ofKind:)` も同じ。
- [reloadAllTimelines()](https://developer.apple.com/documentation/widgetkit/widgetcenter/reloadalltimelines()):
  > Reloads the timelines for all configured widgets belonging to the containing app.
- WWDC20 [Meet WidgetKit](https://developer.apple.com/videos/play/wwdc2020/10028/):
  > You can use the WidgetCenter APIs from within your app process or extension to reload your timeline.
- WWDC21 [Principles of great widgets](https://developer.apple.com/videos/play/wwdc2021/10048/):
  > When using the never policy, you can keep your widget up to date solely with the WidgetCenter reload API from your container or other accessory extensions.

文書化されていないこと:

- WidgetKit が呼び出し元のプロセスを「どの containing app か」とどう判定するか（`Bundle.main`、code signature、team ID など）。
- `.app` の外にある CLI（例: `~/.cargo/bin` に置いたバイナリ）から呼んだ場合の挙動。
- Swift 以外の言語から呼ぶ場合の制約。

推測（未検証）:

- WidgetCenter の API は Swift 専用で、Objective-C には公開されていない。Rust から呼ぶには、`@_cdecl` で C の関数を公開する小さな Swift のライブラリを挟む必要がある。Swift の static lib を Rust にリンクするなら、Swift ランタイム（macOS 10.14.4 以降は OS に同梱）の rpath と、WidgetKit.framework へのリンクが要る。
- 「belonging to the containing app」という書き方から、呼び出し元は widget extension を `Contents/PlugIns` に埋め込んだ `.app` として識別される必要があると考えられる。Rust バイナリがその `.app` の `CFBundleExecutable` として起動され、`Bundle.main` がその `.app` を指していれば、言語に関係なく containing app のプロセスとして扱われるはず。`.app` の外から起動した CLI では、対象のウィジェットが見つからないか、何も起きない可能性が高い。
- unsandboxed の containing app から呼べるかどうかに触れた一次資料も無い。

## 未確認の点

1. macOS 14〜26 のデスクトップウィジェットに 40〜70 回/日の budget が実際に効くか。Apple の一次資料では確かめられなかった。2022 年の「Mac には無い」は Sonoma 以前の発言。
2. macOS で「containing app が foreground」が何を指すか。`LSUIElement` の accessory app が条件を満たすことがあるか。WWDC25 の「running なら budget されない」は macOS にも文字どおり当てはまるのか。
3. `reloadAllTimelines()` が budget で保留されたとき、呼び出し側に何も返らないこと（API が `Void` を返す）は API の形から明らかだが、保留されたかどうかを知る公式の方法は文書化されていない。
4. macOS で WidgetKit Developer Mode（WWDC25 は "in Settings" と言う）に当たる設定がどこにあるか。iOS の開発者設定以外の場所は確かめていない。
5. 呼び出し元のプロセスの識別方法（item 6）と、Rust バイナリを main executable にした `.app` から WidgetCenter を呼んだときの挙動。

実測で確かめる方法の案（未検証）: 実機でメニューバーから `reloadAllTimelines()` を数分おきに呼び、`log stream` で `chronod` と `dasd` のログ（community の報告では `_DASWidgetBudget` の減算が出る）を見る。budget が引かれているかと、reload が保留されているかが分かる。
