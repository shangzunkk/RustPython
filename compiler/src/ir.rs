use indexmap::IndexSet;
use rustpython_bytecode::bytecode::{
    CodeFlags, CodeObject, ConstantData, Instruction, Label, Location,
};

pub type BlockIdx = Label;

pub struct InstructionInfo {
    /// If the instruction has a Label argument, it's actually a BlockIdx, not a code offset
    pub instr: Instruction,
    pub location: Location,
}

pub struct Block {
    pub instructions: Vec<InstructionInfo>,
    pub done: bool,
}
impl Default for Block {
    fn default() -> Self {
        Block {
            instructions: Vec::new(),
            done: false,
        }
    }
}

pub struct CodeInfo {
    pub flags: CodeFlags,
    pub posonlyarg_count: usize, // Number of positional-only arguments
    pub arg_count: usize,
    pub kwonlyarg_count: usize,
    pub source_path: String,
    pub first_line_number: usize,
    pub obj_name: String, // Name of the object that created this code object

    pub blocks: Vec<Block>,
    pub block_order: Vec<BlockIdx>,
    pub constants: Vec<ConstantData>,
    pub name_cache: IndexSet<String>,
    pub varname_cache: IndexSet<String>,
    pub cellvar_cache: IndexSet<String>,
    pub freevar_cache: IndexSet<String>,
}
impl CodeInfo {
    pub fn finalize_code(mut self, optimize: u8) -> CodeObject {
        let max_stacksize = self.max_stacksize();
        let cell2arg = self.cell2arg();

        if optimize > 0 {
            self.dce();
        }

        let CodeInfo {
            flags,
            posonlyarg_count,
            arg_count,
            kwonlyarg_count,
            source_path,
            first_line_number,
            obj_name,

            mut blocks,
            block_order,
            constants,
            name_cache,
            varname_cache,
            cellvar_cache,
            freevar_cache,
        } = self;

        assert!(block_order.len() == blocks.len());

        let mut num_instructions = 0;
        let mut block_to_offset = vec![Label(0); blocks.len()];

        for idx in &block_order {
            let idx = idx.0 as usize;
            block_to_offset[idx] = Label(num_instructions as u32);
            num_instructions += blocks[idx].instructions.len();
        }

        let mut instructions = Vec::with_capacity(num_instructions);
        let mut locations = Vec::with_capacity(num_instructions);

        for idx in block_order {
            let block = std::mem::take(&mut blocks[idx.0 as usize]);
            for mut instr in block.instructions {
                if let Some(l) = instr.instr.label_arg_mut() {
                    *l = block_to_offset[l.0 as usize];
                }
                instructions.push(instr.instr);
                locations.push(instr.location);
            }
        }

        CodeObject {
            flags,
            posonlyarg_count,
            arg_count,
            kwonlyarg_count,
            source_path,
            first_line_number,
            obj_name,

            max_stacksize,
            instructions: instructions.into_boxed_slice(),
            locations: locations.into_boxed_slice(),
            constants: constants.into(),
            names: name_cache.into_iter().collect(),
            varnames: varname_cache.into_iter().collect(),
            cellvars: cellvar_cache.into_iter().collect(),
            freevars: freevar_cache.into_iter().collect(),
            cell2arg,
        }
    }

    fn cell2arg(&self) -> Option<Box<[isize]>> {
        if self.cellvar_cache.is_empty() {
            return None;
        }

        let total_args = self.arg_count
            + self.kwonlyarg_count
            + self.flags.contains(CodeFlags::HAS_VARARGS) as usize
            + self.flags.contains(CodeFlags::HAS_VARKEYWORDS) as usize;

        let mut found_cellarg = false;
        let cell2arg = self
            .cellvar_cache
            .iter()
            .map(|var| {
                self.varname_cache
                    .get_index_of(var)
                    // check that it's actually an arg
                    .filter(|i| *i < total_args)
                    .map_or(-1, |i| {
                        found_cellarg = true;
                        i as isize
                    })
            })
            .collect::<Box<[_]>>();

        if found_cellarg {
            Some(cell2arg)
        } else {
            None
        }
    }

    fn dce(&mut self) {
        for block in &mut self.blocks {
            let mut last_instr = None;
            for (i, ins) in block.instructions.iter().enumerate() {
                if ins.instr.unconditional_branch() {
                    last_instr = Some(i);
                    break;
                }
            }
            if let Some(i) = last_instr {
                block.instructions.truncate(i + 1);
            }
        }
    }

    // TODO: don't use SetupFinally for handling continue/break unwinding, creates
    // too much confusion in stack analysis
    // #[allow(unused)]
    fn max_stacksize(&self) -> u32 {
        let mut maxdepth = 0;
        let mut stack = Vec::with_capacity(self.blocks.len());
        let mut startdepths = vec![0; self.blocks.len()];
        // TODO: 'seen' is kind of a copout for resolving cycles, and it might not even be correct?
        let mut seen = vec![false; self.blocks.len()];
        stack.push((Label(0), 0));
        'process_blocks: while let Some((block, blockorder)) = stack.pop() {
            if seen[block.0 as usize] {
                continue;
            }
            seen[block.0 as usize] = true;
            let mut depth = startdepths[block.0 as usize];
            for i in &self.blocks[block.0 as usize].instructions {
                let instr = &i.instr;
                let effect = instr.stack_effect(false);
                let new_depth = depth + effect;
                if new_depth > maxdepth {
                    maxdepth = new_depth
                }
                if let Some(&target_block) = instr.label_arg() {
                    let effect = instr.stack_effect(true);
                    let target_depth = depth + effect;
                    if target_depth > maxdepth {
                        maxdepth = target_depth
                    }
                    stackdepth_push(
                        &mut stack,
                        &mut startdepths,
                        (target_block, u32::MAX),
                        target_depth,
                    );
                }
                depth = new_depth;
                if instr.unconditional_branch() {
                    continue 'process_blocks;
                }
            }
            seen[block.0 as usize] = false;
            let next_blockorder = if blockorder == u32::MAX {
                self.block_order.iter().position(|x| *x == block).unwrap() as u32 + 1
            } else {
                blockorder + 1
            };
            let next = self.block_order[next_blockorder as usize];
            stackdepth_push(&mut stack, &mut startdepths, (next, next_blockorder), depth);
        }
        maxdepth as u32
    }
}

fn stackdepth_push(
    stack: &mut Vec<(Label, u32)>,
    startdepths: &mut [i32],
    target: (Label, u32),
    depth: i32,
) {
    let block_depth = &mut startdepths[target.0 .0 as usize];
    if depth > *block_depth {
        *block_depth = depth;
        stack.push(target);
    }
}
