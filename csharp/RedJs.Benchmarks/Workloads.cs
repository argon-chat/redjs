namespace RedJs.Benchmarks;

internal sealed record Workload(string Name, string Source, double Argument,
    double Expected, int Operations = 1, string Unit = "run")
{
    // ES5 syntax and explicit strict mode keep the language subset identical.
    internal static readonly Workload[] All =
    [
        new("cold-start", "6 * 7", 0, 42, Unit: "start"),
        new("eval", "6 * 7", 0, 42, Unit: "eval"),
        new("cs-to-js", "(function(n) { 'use strict'; return n + 1; })", 41, 42, Unit: "call"),
        new("js-to-cs", """
            (function(n) {
                'use strict';
                var sum = 0;
                for (var i = 0; i < n; i++) sum = hostAdd(sum, 1);
                return sum;
            })
            """, 10_000, 10_000, 10_000, "callback"),
        new("arithmetic", """
            (function(n) {
                'use strict';
                var sum = 0;
                for (var i = 0; i < n; i++) sum += (i % 97) * (i % 31);
                return sum;
            })
            """, 10_000, ArithmeticSum(10_000)),
        new("fibonacci", """
            (function(n) {
                'use strict';
                function fib(x) { return x < 2 ? x : fib(x - 1) + fib(x - 2); }
                return fib(n);
            })
            """, 20, 6765),
        new("objects", """
            (function(n) {
                'use strict';
                var rows = [];
                for (var i = 0; i < n; i++) rows.push({ x: i, y: i + 1 });
                var sum = 0;
                for (var j = 0; j < rows.length; j++) sum += rows[j].x + rows[j].y;
                return sum;
            })
            """, 1000, 1_000_000),
        new("json", """
            (function(n) {
                'use strict';
                var rows = [];
                for (var i = 0; i < n; i++) {
                    rows.push({ id: i, name: 'row-' + i, score: i * 3, active: i % 2 === 0 });
                }
                var copy = JSON.parse(JSON.stringify(rows));
                var sum = 0;
                for (var j = 0; j < copy.length; j++) sum += copy[j].id + copy[j].score;
                return sum;
            })
            """, 128, 32_512)
    ];

    private static double ArithmeticSum(int n)
    {
        long sum = 0;
        for (int i = 0; i < n; i++) sum += (i % 97) * (i % 31);
        return sum;
    }
}
