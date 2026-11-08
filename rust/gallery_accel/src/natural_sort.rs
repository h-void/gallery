use std::cmp::Ordering;

enum NaturalChunk<'a> {
    Number { normalized: &'a str, len: usize },
    Text(&'a str),
}

struct NaturalChunkIter<'a> {
    rest: &'a str,
}

impl<'a> Iterator for NaturalChunkIter<'a> {
    type Item = NaturalChunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = self.rest.chars().next()?;
        let is_digit = first.is_ascii_digit();
        let split_at = self
            .rest
            .char_indices()
            .find(|&(_, ch)| ch.is_ascii_digit() != is_digit)
            .map(|(idx, _)| idx)
            .unwrap_or(self.rest.len());
        let (chunk, remainder) = self.rest.split_at(split_at);
        self.rest = remainder;
        if is_digit {
            let trimmed = chunk.trim_start_matches('0');
            Some(NaturalChunk::Number {
                normalized: if trimmed.is_empty() { "0" } else { trimmed },
                len: chunk.len(),
            })
        } else {
            Some(NaturalChunk::Text(chunk))
        }
    }
}

fn cmp_lowercase(left: &str, right: &str) -> Ordering {
    if left.is_ascii() && right.is_ascii() {
        left.bytes()
            .map(|b| b.to_ascii_lowercase())
            .cmp(right.bytes().map(|b| b.to_ascii_lowercase()))
    } else {
        left.chars()
            .flat_map(char::to_lowercase)
            .cmp(right.chars().flat_map(char::to_lowercase))
    }
}

pub(crate) fn natural_compare(left: &str, right: &str) -> Ordering {
    let mut left_iter = NaturalChunkIter { rest: left.trim() };
    let mut right_iter = NaturalChunkIter { rest: right.trim() };
    loop {
        match (left_iter.next(), right_iter.next()) {
            (Some(left_chunk), Some(right_chunk)) => {
                let ordering = match (left_chunk, right_chunk) {
                    (
                        NaturalChunk::Number {
                            normalized: left_value,
                            len: left_len,
                        },
                        NaturalChunk::Number {
                            normalized: right_value,
                            len: right_len,
                        },
                    ) => left_value
                        .len()
                        .cmp(&right_value.len())
                        .then_with(|| left_value.cmp(right_value))
                        .then_with(|| left_len.cmp(&right_len)),
                    (NaturalChunk::Text(left_value), NaturalChunk::Text(right_value)) => {
                        cmp_lowercase(left_value, right_value)
                    }
                    (NaturalChunk::Number { .. }, NaturalChunk::Text(_)) => Ordering::Less,
                    (NaturalChunk::Text(_), NaturalChunk::Number { .. }) => Ordering::Greater,
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return cmp_lowercase(left, right),
        }
    }
}
