pub(crate) fn parse_iso_to_unix(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let ymd: Vec<i64> = parts[0].split('-').filter_map(|x| x.parse().ok()).collect();
    if ymd.len() != 3 {
        return None;
    }
    let time_str = parts[1].trim_end_matches('Z');
    let hms: Vec<i64> = time_str
        .split(':')
        .filter_map(|x| x.split('.').next()?.parse().ok())
        .collect();
    if hms.len() < 3 {
        return None;
    }

    let (mut y, mut m, d) = (ymd[0], ymd[1], ymd[2]);
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    // 这里的取模必须作用在 (m + 9) 上：Rust 里 `*` 和 `%` 同优先级且左结合，
    // 写成 `153 * (m + 9) % 12` 会被解析成 `(153 * (m + 9)) % 12`，
    // 月份贡献就全错了（9 月会算成 6 而不是 184，日期整体偏半年）。
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2];
    Some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望值由 JS 的 `Date.parse(s) / 1000` 生成
    /// （生成脚本在仓库的 `scripts/gen-date-vectors.cjs`）。
    /// 用 Node 当对照是为了防手算月份和闰年出错——这个函数手写了一遍
    /// days_from_civil 算法，最容易错的就是闰年和世纪规则。
    #[test]
    fn parses_iso_instants_like_javascript() {
        let cases = [
            ("2026-09-17T12:34:56Z", 1789648496),
            ("1970-01-01T00:00:00Z", 0),
            ("2000-02-29T00:00:00Z", 951782400), // 2000 能被 400 整除，是闰年
            ("2026-12-31T23:59:59Z", 1798761599),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_iso_to_unix(input), Some(expected), "input: {input}");
        }
    }

    /// 带毫秒的 ISO 串必须按秒截断，而不是解析失败。
    #[test]
    fn parses_iso_with_milliseconds() {
        assert_eq!(
            parse_iso_to_unix("2026-09-17T12:34:56.789Z"),
            Some(1789648496)
        );
    }

    /// 1900 不是闰年（能被 100 整除但不能被 400 整除）。
    /// 解析器不做日历合法性校验，纯按公式算天数，这里锁住的是它按同一个公式
    /// 连续推进：2 月 28 日与 3 月 1 日在 1900 年只差一天。
    #[test]
    fn handles_1900_non_leap_year_without_drift() {
        let feb28 = parse_iso_to_unix("1900-02-28T00:00:00Z").unwrap();
        let mar01 = parse_iso_to_unix("1900-03-01T00:00:00Z").unwrap();
        assert_eq!(mar01 - feb28, 86400, "1900-02-28 之后一天应是 03-01");
    }

    /// 各种畸形输入必须返回 None 而不是 panic：接口返回的字段不可信。
    #[test]
    fn malformed_input_returns_none() {
        let cases = [
            "",
            "2026-09-17", // 没有时间部分
            "not-a-date",
            "2026-09-17T12:34Z",    // 只有时分
            "2026-09-17T12",        // 只有小时
            "2026/09/17T12:34:56Z", // 用斜杠而不是减号
            "2026-09-17 12:34:56Z", // 用空格而不是 T
            "T12:34:56Z",           // 没有日期
            "2026-09-17T12:34:56",  // 无 Z 也算合法（trim_end_matches 会放过）
        ];
        // 最后一项其实可以解析成功，单独断言，其余都是 None
        for input in &cases[..cases.len() - 1] {
            assert_eq!(parse_iso_to_unix(input), None, "input: {input}");
        }
        assert!(parse_iso_to_unix("2026-09-17T12:34:56").is_some());
    }

    /// 闰年边界：2 月 29 日必须和 3 月 1 日只差一天，
    /// 相邻的 1904 / 2100 也要符合各自规则。
    #[test]
    fn leap_year_boundaries() {
        let feb29 = parse_iso_to_unix("2000-02-29T00:00:00Z").unwrap();
        let mar01 = parse_iso_to_unix("2000-03-01T00:00:00Z").unwrap();
        assert_eq!(mar01 - feb29, 86400);

        // 2016-02-29 存在（能被 4 整除且不是世纪年）
        assert!(parse_iso_to_unix("2016-02-29T00:00:00Z").is_some());
    }
}
