use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq)]
enum NaturalChunk {
    Number { normalized: String, len: usize },
    Text(String),
}

pub(crate) fn natural_compare(left: &str, right: &str) -> Ordering {
    let left_chunks = natural_chunks(left);
    let right_chunks = natural_chunks(right);
    for (left_chunk, right_chunk) in left_chunks.iter().zip(right_chunks.iter()) {
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
                .then_with(|| left_len.cmp(right_len)),
            (NaturalChunk::Text(left_value), NaturalChunk::Text(right_value)) => {
                left_value.cmp(right_value)
            }
            (NaturalChunk::Number { .. }, NaturalChunk::Text(_)) => Ordering::Less,
            (NaturalChunk::Text(_), NaturalChunk::Number { .. }) => Ordering::Greater,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_chunks
        .len()
        .cmp(&right_chunks.len())
        .then_with(|| left.to_lowercase().cmp(&right.to_lowercase()))
}

fn natural_chunks(value: &str) -> Vec<NaturalChunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_is_digit: Option<bool> = None;
    for ch in value.trim().chars() {
        let is_digit = ch.is_ascii_digit();
        match current_is_digit {
            Some(kind) if kind == is_digit => current.push(ch),
            Some(kind) => {
                push_natural_chunk(&mut chunks, &current, kind);
                current.clear();
                current.push(ch);
                current_is_digit = Some(is_digit);
            }
            None => {
                current.push(ch);
                current_is_digit = Some(is_digit);
            }
        }
    }
    if let Some(kind) = current_is_digit {
        push_natural_chunk(&mut chunks, &current, kind);
    }
    chunks
}

fn push_natural_chunk(chunks: &mut Vec<NaturalChunk>, value: &str, is_digit: bool) {
    if is_digit {
        let trimmed = value.trim_start_matches('0');
        chunks.push(NaturalChunk::Number {
            normalized: if trimmed.is_empty() {
                "0".to_string()
            } else {
                trimmed.to_string()
            },
            len: value.len(),
        });
    } else {
        chunks.push(NaturalChunk::Text(value.to_lowercase()));
    }
}
