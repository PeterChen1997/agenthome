//! 不依赖 schema 的 protobuf 读取：只按字段号取值，供没有公开 `.proto` 的来源（Antigravity CLI）使用。
//!
//! 字段号是对照真实数据逆推出来的，所以这里只提供「取某个字段」的最小能力；
//! 取不到时返回 None，由调用方决定是记警告还是跳过，不在这里猜。

/// 一个字段的值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// 顺序遍历一段消息里的 `(字段号, 值)`。遇到格式错误就停止（后面的字段不可信）。
pub(crate) struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

pub(crate) fn fields(buf: &[u8]) -> Fields<'_> {
    Fields { buf, pos: 0 }
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u32, Wire<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let key = read_varint(self.buf, &mut self.pos)?;
        let field = u32::try_from(key >> 3).ok().filter(|f| *f > 0)?;
        let value = match key & 7 {
            0 => Wire::Varint(read_varint(self.buf, &mut self.pos)?),
            1 => {
                let bytes = self.buf.get(self.pos..self.pos + 8)?;
                self.pos += 8;
                Wire::Fixed64(u64::from_le_bytes(bytes.try_into().ok()?))
            }
            2 => {
                let len = usize::try_from(read_varint(self.buf, &mut self.pos)?).ok()?;
                let bytes = self.buf.get(self.pos..self.pos.checked_add(len)?)?;
                self.pos += len;
                Wire::Bytes(bytes)
            }
            5 => {
                let bytes = self.buf.get(self.pos..self.pos + 4)?;
                self.pos += 4;
                Wire::Fixed32(u32::from_le_bytes(bytes.try_into().ok()?))
            }
            _ => {
                // 组（3/4）早已废弃，其余是非法值：停止，不往下读。
                self.pos = self.buf.len();
                return None;
            }
        };
        Some((field, value))
    }
}

/// 字段 `field` 的最后一个 varint（protobuf 语义：标量字段重复出现时后者生效）。
pub(crate) fn varint(buf: &[u8], field: u32) -> Option<u64> {
    fields(buf)
        .filter_map(|(f, v)| match v {
            Wire::Varint(n) if f == field => Some(n),
            _ => None,
        })
        .last()
}

/// 字段 `field` 的全部 length-delimited 值（重复字段、子消息列表）。
pub(crate) fn all_bytes(buf: &[u8], field: u32) -> Vec<&[u8]> {
    fields(buf)
        .filter_map(|(f, v)| match v {
            Wire::Bytes(b) if f == field => Some(b),
            _ => None,
        })
        .collect()
}

/// 字段 `field` 的第一个 length-delimited 值。
pub(crate) fn bytes(buf: &[u8], field: u32) -> Option<&[u8]> {
    fields(buf).find_map(|(f, v)| match v {
        Wire::Bytes(b) if f == field => Some(b),
        _ => None,
    })
}

/// 沿字段号路径逐层进入子消息，如 `[20, 7]` = 字段 20 里的字段 7。
pub(crate) fn path<'a>(buf: &'a [u8], path: &[u32]) -> Option<&'a [u8]> {
    path.iter().try_fold(buf, |cur, field| bytes(cur, *field))
}

/// 字段 `field` 解成 UTF-8 字符串；不是合法 UTF-8 时为 None。
pub(crate) fn string(buf: &[u8], field: u32) -> Option<String> {
    bytes(buf, field).and_then(|b| std::str::from_utf8(b).ok().map(str::to_string))
}

#[cfg(test)]
pub(crate) mod encode {
    //! 测试用的最小编码器，用来手搓 fixture。

    pub fn varint(mut n: u64, out: &mut Vec<u8>) {
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
    }

    pub fn uint(field: u32, n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(field) << 3, &mut out);
        varint(n, &mut out);
        out
    }

    pub fn bytes(field: u32, b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint((u64::from(field) << 3) | 2, &mut out);
        varint(b.len() as u64, &mut out);
        out.extend_from_slice(b);
        out
    }

    pub fn msg(field: u32, parts: &[Vec<u8>]) -> Vec<u8> {
        bytes(field, &parts.concat())
    }
}

#[cfg(test)]
mod tests {
    use super::encode as e;
    use super::*;

    #[test]
    fn 按路径取嵌套字段与重复字段() {
        let buf = [
            e::uint(1, 300),
            e::msg(
                20,
                &[
                    e::bytes(1, "你好".as_bytes()),
                    e::msg(7, &[e::bytes(2, b"read_file")]),
                    e::msg(7, &[e::bytes(2, b"run_command")]),
                ],
            ),
        ]
        .concat();
        assert_eq!(varint(&buf, 1), Some(300));
        let p = path(&buf, &[20]).unwrap();
        assert_eq!(string(p, 1).as_deref(), Some("你好"));
        let names: Vec<_> = all_bytes(p, 7)
            .into_iter()
            .filter_map(|c| string(c, 2))
            .collect();
        assert_eq!(names, ["read_file", "run_command"]);
        assert_eq!(path(&buf, &[20, 9]), None);
    }

    #[test]
    fn 截断的数据不越界也不编造字段() {
        let mut buf = e::bytes(3, b"hello");
        buf.truncate(4);
        assert_eq!(bytes(&buf, 3), None);
        assert_eq!(fields(&[0xff, 0xff]).count(), 0);
    }
}
